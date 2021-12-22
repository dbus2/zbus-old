use async_broadcast::Receiver;
use async_channel::bounded;
use async_executor::Task;
use event_listener::{Event, EventListener};
use futures_core::{ready, stream};
use futures_util::{future::Either, stream::FilterMap};
use once_cell::sync::OnceCell;
use ordered_stream::{join as join_streams, FromFuture, OrderedStream, PollResult};
use static_assertions::assert_impl_all;
use std::{
    collections::HashMap,
    convert::{TryFrom, TryInto},
    future::{Future, Ready},
    ops::Deref,
    pin::Pin,
    sync::{Arc, RwLock, RwLockReadGuard},
    task::{Context, Poll},
};

use zbus_names::{BusName, InterfaceName, MemberName, OwnedUniqueName, UniqueName, WellKnownName};
use zvariant::{ObjectPath, Optional, OwnedValue, Value};

use crate::{
    fdo::{self, IntrospectableProxy, PropertiesProxy},
    CacheProperties, Connection, Error, Message, MessageBuilder, MessageSequence, ProxyBuilder,
    Result,
};

#[derive(Debug, Default)]
struct PropertyValue {
    value: Option<OwnedValue>,
    event: Event,
}

#[derive(Debug)]
pub(crate) struct PropertiesCache {
    values: RwLock<HashMap<String, PropertyValue>>,
    ready: async_channel::Receiver<Result<()>>,
}

/// A client-side interface proxy.
///
/// A `Proxy` is a helper to interact with an interface on a remote object.
///
/// # Example
///
/// ```
/// use std::result::Result;
/// use std::error::Error;
/// use async_io::block_on;
/// use zbus::{Connection, Proxy};
///
/// fn main() -> Result<(), Box<dyn Error>> {
///     block_on(run())
/// }
///
/// async fn run() -> Result<(), Box<dyn Error>> {
///     let connection = Connection::session().await?;
///     let p = Proxy::new(
///         &connection,
///         "org.freedesktop.DBus",
///         "/org/freedesktop/DBus",
///         "org.freedesktop.DBus",
///     ).await?;
///     // owned return value
///     let _id: String = p.call("GetId", &()).await?;
///     // borrowed return value
///     let _id: &str = p.call_method("GetId", &()).await?.body()?;
///
///     Ok(())
/// }
/// ```
///
/// # Note
///
/// It is recommended to use the [`dbus_proxy`] macro, which provides a more convenient and
/// type-safe *façade* `Proxy` derived from a Rust trait.
///
/// ## Current limitations:
///
/// At the moment, `Proxy` doesn't:
///
/// * cache properties
/// * track the current name owner
/// * prevent auto-launching
///
/// [`futures` crate]: https://crates.io/crates/futures
/// [`dbus_proxy`]: attr.dbus_proxy.html
#[derive(Clone, Debug)]
pub struct Proxy<'a> {
    pub(crate) inner: Arc<ProxyInner<'a>>,
}

assert_impl_all!(Proxy<'_>: Send, Sync, Unpin);

/// This is required to avoid having the Drop impl extend the lifetime 'a, which breaks zbus_xmlgen
/// (and possibly other crates).
#[derive(derivative::Derivative)]
#[derivative(Debug)]
pub(crate) struct ProxyInnerStatic {
    #[derivative(Debug = "ignore")]
    pub(crate) conn: Connection,
    dest_name_watcher: OnceCell<String>,
}

#[derive(Debug)]
pub(crate) struct ProxyInner<'a> {
    inner_without_borrows: ProxyInnerStatic,
    pub(crate) destination: BusName<'a>,
    pub(crate) path: ObjectPath<'a>,
    pub(crate) interface: InterfaceName<'a>,

    property_cache: Option<OnceCell<(Arc<PropertiesCache>, Task<()>)>>,
}

impl Drop for ProxyInnerStatic {
    fn drop(&mut self) {
        if let Some(expr) = self.dest_name_watcher.take() {
            self.conn.queue_remove_match(expr);
        }
    }
}

/// A property changed event.
///
/// The property changed event generated by [`PropertyStream`].
pub struct PropertyChanged<'a, T> {
    name: &'a str,
    properties: Arc<PropertiesCache>,
    proxy: Proxy<'a>,
    phantom: std::marker::PhantomData<T>,
}

impl<'a, T> PropertyChanged<'a, T> {
    // The name of the property that changed.
    pub fn name(&self) -> &str {
        self.name
    }

    // Get the raw value of the property that changed.
    //
    // If the notification signal contained the new value, it has been cached already and this call
    // will return that value. Otherwise (i-e invalidated property), a D-Bus call is made to fetch
    // and cache the new value.
    pub async fn get_raw<'p>(&'p self) -> Result<impl Deref<Target = Value<'static>> + 'p> {
        struct Wrapper<'w> {
            name: &'w str,
            values: RwLockReadGuard<'w, HashMap<String, PropertyValue>>,
        }

        impl<'w> Deref for Wrapper<'w> {
            type Target = Value<'static>;

            fn deref(&self) -> &Self::Target {
                &*self
                    .values
                    .get(self.name)
                    .expect("PropertyStream with no corresponding property")
                    .value
                    .as_ref()
                    .expect("PropertyStream with no corresponding property")
            }
        }

        {
            let values = self.properties.values.read().expect("lock poisoned");
            if values
                .get(self.name)
                .expect("PropertyStream with no corresponding property")
                .value
                .is_some()
            {
                return Ok(Wrapper {
                    name: self.name,
                    values,
                });
            }
        }

        // The property was invalidated, so we need to fetch the new value.
        let properties_proxy = self.proxy.properties_proxy();
        let value = properties_proxy
            .get(self.proxy.inner.interface.clone(), self.name)
            .await
            .map_err(crate::Error::from)?;

        // Save the new value
        let mut values = self.properties.values.write().expect("lock poisoned");

        values
            .get_mut(self.name)
            .expect("PropertyStream with no corresponding property")
            .value = Some(value);

        Ok(Wrapper {
            name: self.name,
            values: self.properties.values.read().expect("lock poisoned"),
        })
    }
}

impl<T> PropertyChanged<'_, T>
where
    T: TryFrom<zvariant::OwnedValue>,
    T::Error: Into<crate::Error>,
{
    // Get the value of the property that changed.
    //
    // If the notification signal contained the new value, it has been cached already and this call
    // will return that value. Otherwise (i-e invalidated property), a D-Bus call is made to fetch
    // and cache the new value.
    pub async fn get(&self) -> Result<T> {
        self.get_raw()
            .await
            .and_then(|v| T::try_from(OwnedValue::from(&*v)).map_err(Into::into))
    }
}

/// A [`stream::Stream`] implementation that yields property change notifications.
///
/// Use [`Proxy::receive_property_changed`] to create an instance of this type.
#[derive(derivative::Derivative)]
#[derivative(Debug)]
pub struct PropertyStream<'a, T> {
    name: &'a str,
    proxy: Proxy<'a>,
    event: EventListener,
    phantom: std::marker::PhantomData<T>,
}

impl<'a, T> stream::Stream for PropertyStream<'a, T>
where
    T: Unpin,
{
    type Item = PropertyChanged<'a, T>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let m = self.get_mut();
        let properties = match m.proxy.get_property_cache() {
            Some(properties) => properties.clone(),
            // With no cache, we will get no updates; return immediately
            None => return Poll::Ready(None),
        };
        ready!(Pin::new(&mut m.event).poll(cx));

        m.event = properties
            .values
            .read()
            .expect("lock poisoned")
            .get(m.name)
            .expect("PropertyStream with no corresponding property")
            .event
            .listen();

        Poll::Ready(Some(PropertyChanged {
            name: m.name,
            properties,
            proxy: m.proxy.clone(),
            phantom: std::marker::PhantomData,
        }))
    }
}

impl PropertiesCache {
    fn update_cache(&self, changed: &HashMap<&str, Value<'_>>, invalidated: Vec<&str>) {
        let mut values = self.values.write().expect("lock poisoned");

        for inval in invalidated {
            if let Some(entry) = values.get_mut(&*inval) {
                entry.value = None;
                entry.event.notify(usize::MAX);
            }
        }

        for (property_name, value) in changed {
            let entry = values
                .entry(property_name.to_string())
                .or_insert_with(PropertyValue::default);

            entry.value = Some(OwnedValue::from(value));
            entry.event.notify(usize::MAX);
        }
    }

    /// Wait for the cache to be populated and return any error encountered during population
    pub(crate) async fn ready(&self) -> Result<()> {
        // Only one caller will actually get the result; all other callers see a closed channel,
        // but that indicates the cache is ready (and on error, the cache will be empty, which just
        // means we bypass it)
        match self.ready.recv().await {
            Ok(res) => res,
            Err(_closed) => Ok(()),
        }
    }
}

impl<'a> ProxyInner<'a> {
    pub(crate) fn new(
        conn: Connection,
        destination: BusName<'a>,
        path: ObjectPath<'a>,
        interface: InterfaceName<'a>,
        cache: CacheProperties,
    ) -> Self {
        let property_cache = match cache {
            CacheProperties::Yes | CacheProperties::Lazily => Some(OnceCell::new()),
            CacheProperties::No => None,
        };
        Self {
            inner_without_borrows: ProxyInnerStatic {
                conn,
                dest_name_watcher: OnceCell::new(),
            },
            destination,
            path,
            interface,
            property_cache,
        }
    }

    /// Resolves the destination name to the associated unique connection name and watches for any changes.
    ///
    /// Typically you would want to create the [`Proxy`] with the well-known name of the destination
    /// service but signal messages only specify the unique name of the peer (except for signals
    /// from `org.freedesktop.DBus` service). This means we have no means to check the sender of
    /// the message. While in most cases this will not be a problem, it becomes a problem if you
    /// need to communicate with multiple services exposing the same interface, over the same
    /// connection. Hence the need for this method.
    ///
    /// This is only called when the user show interest in receiving a signal so that we don't end up
    /// doing all this needlessly.
    pub(crate) async fn destination_unique_name(&self) -> Result<()> {
        if !self.inner_without_borrows.conn.is_bus() {
            // Names don't mean much outside the bus context.
            return Ok(());
        }

        if let BusName::WellKnown(well_known_name) = &self.destination {
            if self.inner_without_borrows.dest_name_watcher.get().is_some() {
                // Already watching over the bus for any name updates so nothing to do here.
                return Ok(());
            }

            let conn = &self.inner_without_borrows.conn;
            let signal_expr = format!(
                concat!(
                    "type='signal',",
                    "sender='org.freedesktop.DBus',",
                    "path='/org/freedesktop/DBus',",
                    "interface='org.freedesktop.DBus',",
                    "member='NameOwnerChanged',",
                    "arg0='{}'"
                ),
                well_known_name
            );

            conn.add_match(signal_expr.clone()).await?;

            if self
                .inner_without_borrows
                .dest_name_watcher
                .set(signal_expr.clone())
                .is_err()
            {
                // we raced another destination_unique_name call and added it twice
                conn.remove_match(signal_expr).await?;
            }
        }

        Ok(())
    }
}

impl<'a> Proxy<'a> {
    /// Create a new `Proxy` for the given destination/path/interface.
    pub async fn new<D, P, I>(
        conn: &Connection,
        destination: D,
        path: P,
        interface: I,
    ) -> Result<Proxy<'a>>
    where
        D: TryInto<BusName<'a>>,
        P: TryInto<ObjectPath<'a>>,
        I: TryInto<InterfaceName<'a>>,
        D::Error: Into<Error>,
        P::Error: Into<Error>,
        I::Error: Into<Error>,
    {
        ProxyBuilder::new_bare(conn)
            .destination(destination)?
            .path(path)?
            .interface(interface)?
            .build()
            .await
    }

    /// Create a new `Proxy` for the given destination/path/interface, taking ownership of all
    /// passed arguments.
    pub async fn new_owned<D, P, I>(
        conn: Connection,
        destination: D,
        path: P,
        interface: I,
    ) -> Result<Proxy<'a>>
    where
        D: TryInto<BusName<'static>>,
        P: TryInto<ObjectPath<'static>>,
        I: TryInto<InterfaceName<'static>>,
        D::Error: Into<Error>,
        P::Error: Into<Error>,
        I::Error: Into<Error>,
    {
        ProxyBuilder::new_bare(&conn)
            .destination(destination)?
            .path(path)?
            .interface(interface)?
            .build()
            .await
    }

    /// Get a reference to the associated connection.
    pub fn connection(&self) -> &Connection {
        &self.inner.inner_without_borrows.conn
    }

    /// Get a reference to the destination service name.
    pub fn destination(&self) -> &BusName<'_> {
        &self.inner.destination
    }

    /// Get a reference to the object path.
    pub fn path(&self) -> &ObjectPath<'_> {
        &self.inner.path
    }

    /// Get a reference to the interface.
    pub fn interface(&self) -> &InterfaceName<'_> {
        &self.inner.interface
    }

    /// Introspect the associated object, and return the XML description.
    ///
    /// See the [xml](xml/index.html) module for parsing the result.
    pub async fn introspect(&self) -> fdo::Result<String> {
        let proxy = IntrospectableProxy::builder(&self.inner.inner_without_borrows.conn)
            .destination(&self.inner.destination)?
            .path(&self.inner.path)?
            .build()
            .await?;

        proxy.introspect().await
    }

    fn properties_proxy(&self) -> PropertiesProxy<'_> {
        PropertiesProxy::builder(&self.inner.inner_without_borrows.conn)
            // Safe because already checked earlier
            .destination(self.inner.destination.as_ref())
            .unwrap()
            // Safe because already checked earlier
            .path(self.inner.path.as_ref())
            .unwrap()
            // does not have properties
            .cache_properties(CacheProperties::No)
            .build_internal()
            .into()
    }

    fn owned_properties_proxy(&self) -> PropertiesProxy<'static> {
        PropertiesProxy::builder(&self.inner.inner_without_borrows.conn)
            // Safe because already checked earlier
            .destination(self.inner.destination.to_owned())
            .unwrap()
            // Safe because already checked earlier
            .path(self.inner.path.to_owned())
            .unwrap()
            // does not have properties
            .cache_properties(CacheProperties::No)
            .build_internal()
            .into()
    }

    /// Get the cache, starting it in the background if needed.
    ///
    /// Use PropertiesCache::ready() to wait for the cache to be populated and to get any errors
    /// encountered in the population.
    pub(crate) fn get_property_cache(&self) -> Option<&Arc<PropertiesCache>> {
        use ordered_stream::OrderedStreamExt;
        let cache = match &self.inner.property_cache {
            Some(cache) => cache,
            None => return None,
        };
        let (cache, _) = &cache.get_or_init(|| {
            let proxy = self.owned_properties_proxy();
            let (send, recv) = bounded(1);

            let cache = Arc::new(PropertiesCache {
                values: Default::default(),
                ready: recv,
            });

            let interface = self.interface().to_owned();
            let properties = cache.clone();

            let task = self.connection().executor().spawn(async move {
                let prop_changes = match proxy.receive_properties_changed().await {
                    Ok(s) => s.map(Either::Left),
                    Err(e) => {
                        // ignore send errors, it just means the original future was cancelled
                        let _ = send.send(Err(e)).await;
                        return;
                    }
                };

                let get_all = MessageBuilder::method_call(proxy.path().as_ref(), "GetAll")
                    .unwrap()
                    .destination(proxy.destination())
                    .unwrap()
                    .interface(proxy.interface())
                    .unwrap()
                    .build(&interface)
                    .unwrap();

                let get_all = match proxy.connection().call_method_raw(get_all).await {
                    Ok(s) => FromFuture::from(s).map(Either::Right),
                    Err(e) => {
                        let _ = send.send(Err(e)).await;
                        return;
                    }
                };

                let mut join = join_streams(prop_changes, get_all);

                loop {
                    match join.next().await {
                        Some(Either::Left(update)) => {
                            if let Ok(args) = update.args() {
                                if args.interface_name == interface {
                                    properties.update_cache(
                                        &args.changed_properties,
                                        args.invalidated_properties,
                                    );
                                }
                            }
                        }
                        Some(Either::Right(Ok(populate))) => {
                            let result = populate
                                .body()
                                .map(|values| properties.update_cache(&values, Vec::new()));
                            let _ = send.send(result).await;
                            send.close();
                        }
                        Some(Either::Right(Err(e))) => {
                            let _ = send.send(Err(e)).await;
                            send.close();
                        }
                        None => return,
                    }
                }
            });

            (cache, task)
        });

        Some(cache)
    }

    /// Get the cached value of the property `property_name`.
    ///
    /// This returns `None` if the property is not in the cache.  This could be because the cache
    /// was invalidated by an update, because caching was disabled for this property or proxy, or
    /// because the cache has not yet been populated.  Use `get_property` to fetch the value from
    /// the peer.
    pub fn cached_property<T>(&self, property_name: &str) -> Result<Option<T>>
    where
        T: TryFrom<OwnedValue>,
        T::Error: Into<Error>,
    {
        self.cached_property_raw(property_name)
            .as_deref()
            .map(|v| T::try_from(OwnedValue::from(v)))
            .transpose()
            .map_err(Into::into)
    }

    /// Get the cached value of the property `property_name`.
    ///
    /// Same as `cached_property`, but gives you access to the raw value stored in the cache. This
    /// is useful if you want to avoid allocations and cloning.
    pub fn cached_property_raw<'p>(
        &'p self,
        property_name: &'p str,
    ) -> Option<impl Deref<Target = Value<'static>> + 'p> {
        if let Some(values) = self
            .inner
            .property_cache
            .as_ref()
            .and_then(OnceCell::get)
            .map(|c| c.0.values.read().expect("lock poisoned"))
        {
            // ensure that the property is in the cache.
            values.get(property_name)?;

            struct Wrapper<'a> {
                values: RwLockReadGuard<'a, HashMap<String, PropertyValue>>,
                property_name: &'a str,
            }

            impl Deref for Wrapper<'_> {
                type Target = Value<'static>;

                fn deref(&self) -> &Self::Target {
                    self.values
                        .get(self.property_name)
                        .and_then(|e| e.value.as_ref())
                        .map(|v| v.deref())
                        .expect("inexistent property")
                }
            }

            Some(Wrapper {
                values,
                property_name,
            })
        } else {
            None
        }
    }

    async fn get_proxy_property(&self, property_name: &str) -> Result<OwnedValue> {
        Ok(self
            .properties_proxy()
            .get(self.inner.interface.as_ref(), property_name)
            .await?)
    }

    /// Get the property `property_name`.
    ///
    /// Get the property value from the cache (if caching is enabled on this proxy) or call the
    /// `Get` method of the `org.freedesktop.DBus.Properties` interface.
    pub async fn get_property<T>(&self, property_name: &str) -> Result<T>
    where
        T: TryFrom<OwnedValue>,
        T::Error: Into<Error>,
    {
        if let Some(cache) = self.get_property_cache() {
            cache.ready().await?;
        }
        if let Some(value) = self.cached_property(property_name)? {
            return Ok(value);
        }

        let value = self.get_proxy_property(property_name).await?;
        value.try_into().map_err(Into::into)
    }

    /// Set the property `property_name`.
    ///
    /// Effectively, call the `Set` method of the `org.freedesktop.DBus.Properties` interface.
    pub async fn set_property<'t, T: 't>(&self, property_name: &str, value: T) -> fdo::Result<()>
    where
        T: Into<Value<'t>>,
    {
        self.properties_proxy()
            .set(self.inner.interface.as_ref(), property_name, &value.into())
            .await
    }

    /// Call a method and return the reply.
    ///
    /// Typically, you would want to use [`call`] method instead. Use this method if you need to
    /// deserialize the reply message manually (this way, you can avoid the memory
    /// allocation/copying, by deserializing the reply to an unowned type).
    ///
    /// [`call`]: struct.Proxy.html#method.call
    pub async fn call_method<'m, M, B>(&self, method_name: M, body: &B) -> Result<Arc<Message>>
    where
        M: TryInto<MemberName<'m>>,
        M::Error: Into<Error>,
        B: serde::ser::Serialize + zvariant::DynamicType,
    {
        self.inner
            .inner_without_borrows
            .conn
            .call_method(
                Some(&self.inner.destination),
                self.inner.path.as_str(),
                Some(&self.inner.interface),
                method_name,
                body,
            )
            .await
    }

    /// Call a method and return the reply body.
    ///
    /// Use [`call_method`] instead if you need to deserialize the reply manually/separately.
    ///
    /// [`call_method`]: struct.Proxy.html#method.call_method
    pub async fn call<'m, M, B, R>(&self, method_name: M, body: &B) -> Result<R>
    where
        M: TryInto<MemberName<'m>>,
        M::Error: Into<Error>,
        B: serde::ser::Serialize + zvariant::DynamicType,
        R: serde::de::DeserializeOwned + zvariant::Type,
    {
        let reply = self.call_method(method_name, body).await?;

        Ok(reply.body()?)
    }

    /// Call a method without expecting a reply
    ///
    /// This sets the `NoReplyExpected` flag on the calling message and does not wait for a reply.
    pub async fn call_noreply<'m, M, B>(&self, method_name: M, body: &B) -> Result<()>
    where
        M: TryInto<MemberName<'m>>,
        M::Error: Into<Error>,
        B: serde::ser::Serialize + zvariant::DynamicType,
    {
        let msg = MessageBuilder::method_call(self.inner.path.as_ref(), method_name)?
            .with_flags(zbus::MessageFlags::NoReplyExpected)?
            .destination(&self.inner.destination)?
            .interface(&self.inner.interface)?
            .build(body)?;

        self.inner
            .inner_without_borrows
            .conn
            .send_message(msg)
            .await?;

        Ok(())
    }

    /// Create a stream for signal named `signal_name`.
    pub async fn receive_signal<'m: 'a, M>(&self, signal_name: M) -> Result<SignalStream<'a>>
    where
        M: TryInto<MemberName<'m>>,
        M::Error: Into<Error>,
    {
        let signal_name = signal_name.try_into().map_err(Into::into)?;
        self.receive_signals(Some(signal_name)).await
    }

    async fn receive_signals<'m: 'a>(
        &self,
        signal_name: Option<MemberName<'m>>,
    ) -> Result<SignalStream<'a>> {
        // Time to try & resolve the destination name & track changes to it.
        let conn = self.inner.inner_without_borrows.conn.clone();
        let stream = conn.msg_receiver.activate_cloned();
        self.inner.destination_unique_name().await?;

        let mut expr = format!(
            "type='signal',sender='{}',path='{}',interface='{}'",
            self.destination(),
            self.path(),
            self.interface(),
        );
        if let Some(name) = &signal_name {
            use std::fmt::Write;
            write!(expr, ",member='{}'", name).unwrap();
        }
        conn.add_match(expr.clone()).await?;

        let (src_bus_name, src_unique_name, src_query) = match self.destination().to_owned() {
            BusName::Unique(name) => (None, Some(name), None),
            BusName::WellKnown(name) => {
                let id = conn
                    .send_message(
                        MessageBuilder::method_call("/org/freedesktop/DBus", "GetNameOwner")?
                            .destination("org.freedesktop.DBus")?
                            .interface("org.freedesktop.DBus")?
                            .build(&name)?,
                    )
                    .await?;
                (Some(name), None, Some(id))
            }
        };

        Ok(SignalStream {
            stream,
            proxy: self.clone(),
            expr,
            src_bus_name,
            src_query,
            src_unique_name,
            member: signal_name,
            last_seq: MessageSequence::default(),
        })
    }

    /// Create a stream for all signals emitted by this service.
    pub async fn receive_all_signals(&self) -> Result<SignalStream<'a>> {
        self.receive_signals(None).await
    }

    /// Get a stream to receive property changed events.
    ///
    /// Note that zbus doesn't queue the updates. If the listener is slower than the receiver, it
    /// will only receive the last update.
    ///
    /// If caching is not enabled on this proxy, the resulting stream will not return any events.
    pub async fn receive_property_changed<'name: 'a, T>(
        &self,
        name: &'name str,
    ) -> PropertyStream<'a, T> {
        let properties = self.get_property_cache();
        let event = if let Some(properties) = &properties {
            let mut values = properties.values.write().expect("lock poisoned");
            let entry = values
                .entry(name.to_string())
                .or_insert_with(PropertyValue::default);
            entry.event.listen()
        } else {
            Event::new().listen()
        };

        PropertyStream {
            name,
            proxy: self.clone(),
            event,
            phantom: std::marker::PhantomData,
        }
    }

    /// Get a stream to receive destination owner changed events.
    ///
    /// If the proxy destination is a unique name, the stream will be notified of the peer
    /// disconnection from the bus (with a `None` value).
    ///
    /// If the proxy destination is a well-known name, the stream will be notified whenever the name
    /// owner is changed, either by a new peer being granted ownership (`Some` value) or when the
    /// name is released (with a `None` value).
    ///
    /// Note that zbus doesn't queue the updates. If the listener is slower than the receiver, it
    /// will only receive the last update.
    pub async fn receive_owner_changed(&self) -> Result<OwnerChangedStream<'_>> {
        use futures_util::StreamExt;
        use std::future::ready;

        let dbus_proxy = fdo::DBusProxy::builder(self.connection())
            .cache_properties(CacheProperties::No)
            .build()
            .await?;
        let dest = self.destination().to_owned();
        Ok(OwnerChangedStream {
            filter_stream: dbus_proxy
                // TODO: this signal stream matches all arguments, but we are interested only by a
                // specific arg0. Rewrite with a custom match, or teach a signal stream to filter by
                // arg?
                .receive_name_owner_changed()
                .await?
                .filter_map(Box::new(move |signal| {
                    let args = signal.args().unwrap();

                    if args.name() != &dest {
                        return ready(None);
                    }
                    let new_owner = args.new_owner().as_ref().map(|owner| owner.to_owned());
                    ready(Some(new_owner))
                })),
            name: self.destination().clone(),
        })
    }
}

type OwnerChangedStreamFilter<'a> = FilterMap<
    fdo::NameOwnerChangedStream<'a>,
    Ready<Option<Option<UniqueName<'static>>>>,
    Box<dyn FnMut(fdo::NameOwnerChanged) -> Ready<Option<Option<UniqueName<'static>>>>>,
>;

/// A [`stream::Stream`] implementation that yields `UniqueName` when the bus owner changes.
///
/// Use [`Proxy::receive_owner_changed`] to create an instance of this type.
pub struct OwnerChangedStream<'a> {
    filter_stream: OwnerChangedStreamFilter<'a>,
    name: BusName<'a>,
}

impl OwnerChangedStream<'_> {
    /// The bus name being tracked.
    pub fn name(&self) -> &BusName<'_> {
        &self.name
    }
}

impl<'a> stream::Stream for OwnerChangedStream<'a> {
    type Item = Option<UniqueName<'static>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        use futures_util::StreamExt;
        self.get_mut().filter_stream.poll_next_unpin(cx)
    }
}

/// A [`stream::Stream`] implementation that yields signal [messages](`Message`).
///
/// Use [`Proxy::receive_signal`] to create an instance of this type.
#[derive(Debug)]
pub struct SignalStream<'a> {
    stream: Receiver<Arc<Message>>,
    proxy: Proxy<'a>,
    expr: String,
    src_bus_name: Option<WellKnownName<'a>>,
    src_query: Option<u32>,
    src_unique_name: Option<UniqueName<'static>>,
    member: Option<MemberName<'a>>,
    last_seq: MessageSequence,
}

impl<'a> SignalStream<'a> {
    fn filter(&mut self, msg: &Message) -> Result<bool> {
        if msg.message_type() == zbus::MessageType::MethodReturn
            && self.src_query.is_some()
            && msg.reply_serial() == self.src_query
        {
            self.src_query = None;
            self.src_unique_name = Some(OwnedUniqueName::into(msg.body()?));
        }
        if msg.message_type() != zbus::MessageType::Signal {
            return Ok(false);
        }
        let memb = msg.member();
        let iface = msg.interface();
        let path = msg.path();

        if (self.member.is_none() || memb == self.member)
            && path.as_ref() == Some(self.proxy.path())
            && iface.as_ref() == Some(self.proxy.interface())
        {
            let header = msg.header()?;
            let sender = header.sender()?;
            if sender == self.src_unique_name.as_ref() {
                return Ok(true);
            }
        }

        // The src_unique_name must be maintained in lock-step with the applied filter
        if let Some(bus_name) = &self.src_bus_name {
            if memb.as_deref() == Some("NameOwnerChanged")
                && iface.as_deref() == Some("org.freedesktop.DBus")
                && path.as_deref() == Some("/org/freedesktop/DBus")
            {
                let header = msg.header()?;
                if let Ok(Some(sender)) = header.sender() {
                    if sender == "org.freedesktop.DBus" {
                        let (name, _, new_owner) = msg.body::<(
                            WellKnownName<'_>,
                            Optional<UniqueName<'_>>,
                            Optional<UniqueName<'_>>,
                        )>()?;

                        if &name == bus_name {
                            self.src_unique_name = new_owner.as_ref().map(|n| n.to_owned());
                        }
                    }
                }
            }
        }

        Ok(false)
    }
}

assert_impl_all!(SignalStream<'_>: Send, Sync, Unpin);

impl<'a> stream::Stream for SignalStream<'a> {
    type Item = Arc<Message>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        while let Some(msg) = ready!(Pin::new(&mut this.stream).poll_next(cx)) {
            this.last_seq = msg.recv_position();
            if let Ok(true) = this.filter(&msg) {
                return Poll::Ready(Some(msg));
            }
        }
        Poll::Ready(None)
    }
}

impl<'a> OrderedStream for SignalStream<'a> {
    type Data = Arc<Message>;
    type Ordering = MessageSequence;

    fn poll_next_before(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        before: Option<&Self::Ordering>,
    ) -> Poll<PollResult<Self::Ordering, Self::Data>> {
        let this = self.get_mut();
        loop {
            if let Some(before) = before {
                if this.last_seq >= *before {
                    return Poll::Ready(PollResult::NoneBefore);
                }
            }
            if let Some(msg) = ready!(stream::Stream::poll_next(Pin::new(&mut this.stream), cx)) {
                this.last_seq = msg.recv_position();
                if let Ok(true) = this.filter(&msg) {
                    return Poll::Ready(PollResult::Item {
                        data: msg,
                        ordering: this.last_seq,
                    });
                }
            } else {
                return Poll::Ready(PollResult::Terminated);
            }
        }
    }
}

impl<'a> stream::FusedStream for SignalStream<'a> {
    fn is_terminated(&self) -> bool {
        self.stream.is_terminated()
    }
}

impl<'a> std::ops::Drop for SignalStream<'a> {
    fn drop(&mut self) {
        self.proxy
            .connection()
            .queue_remove_match(std::mem::take(&mut self.expr));
    }
}

impl<'a> From<crate::blocking::Proxy<'a>> for Proxy<'a> {
    fn from(proxy: crate::blocking::Proxy<'a>) -> Self {
        proxy.into_inner()
    }
}

#[cfg(test)]
mod tests {
    use zbus_names::UniqueName;

    use super::*;
    use async_io::block_on;
    use ntest::timeout;
    use std::future::ready;
    use test_log::test;
    use zvariant::Optional;

    #[test]
    #[timeout(15000)]
    fn signal() {
        block_on(test_signal()).unwrap();
    }

    async fn test_signal() -> Result<()> {
        use futures_util::StreamExt;
        // Register a well-known name with the session bus and ensure we get the appropriate
        // signals called for that.
        let conn = Connection::session().await?;
        let unique_name = conn.unique_name().unwrap();

        let proxy = fdo::DBusProxy::new(&conn).await?;

        let well_known = "org.freedesktop.zbus.async.ProxySignalStreamTest";
        let owner_changed_stream = proxy
            .receive_signal("NameOwnerChanged")
            .await?
            .filter(|msg| {
                if let Ok((name, _, new_owner)) = msg.body::<(
                    BusName<'_>,
                    Optional<UniqueName<'_>>,
                    Optional<UniqueName<'_>>,
                )>() {
                    ready(match &*new_owner {
                        Some(new_owner) => *new_owner == *unique_name && name == well_known,
                        None => false,
                    })
                } else {
                    ready(false)
                }
            });

        let name_acquired_stream = proxy.receive_signal("NameAcquired").await?.filter(|msg| {
            if let Ok(name) = msg.body::<BusName<'_>>() {
                return ready(name == well_known);
            }

            ready(false)
        });

        let _prop_stream =
            proxy
                .receive_property_changed("SomeProp")
                .await
                .filter_map(|changed| async move {
                    let v: Option<u32> = changed.get().await.ok();
                    dbg!(v)
                });

        let reply = proxy
            .request_name(
                well_known.try_into()?,
                fdo::RequestNameFlags::ReplaceExisting.into(),
            )
            .await?;
        assert_eq!(reply, fdo::RequestNameReply::PrimaryOwner);

        let (changed_signal, acquired_signal) = futures_util::join!(
            owner_changed_stream.into_future(),
            name_acquired_stream.into_future(),
        );

        let changed_signal = changed_signal.0.unwrap();
        let (acquired_name, _, new_owner) = changed_signal
            .body::<(
                BusName<'_>,
                Optional<UniqueName<'_>>,
                Optional<UniqueName<'_>>,
            )>()
            .unwrap();
        assert_eq!(acquired_name, well_known);
        assert_eq!(*new_owner.as_ref().unwrap(), *unique_name);

        let acquired_signal = acquired_signal.0.unwrap();
        assert_eq!(acquired_signal.body::<&str>().unwrap(), well_known);

        Ok(())
    }
}
