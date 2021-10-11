use async_broadcast::{broadcast, InactiveReceiver, Sender as Broadcaster};
use async_channel::{bounded, Receiver, Sender};
use async_executor::Executor;
use async_io::block_on;
use async_lock::{Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};
use async_task::Task;
use event_listener::EventListener;
use futures_core::future::BoxFuture;
use futures_util::stream::{FuturesUnordered, Stream};
use once_cell::sync::OnceCell;
use slotmap::DenseSlotMap;
use static_assertions::assert_impl_all;
use std::{
    collections::{HashMap, HashSet},
    convert::TryInto,
    future::ready,
    io::{self, ErrorKind},
    ops::{Deref, DerefMut},
    pin::Pin,
    sync::{
        self,
        atomic::{AtomicU32, Ordering::SeqCst},
        Arc, Weak,
    },
    task::{Context, Poll},
};
use zbus_names::{BusName, ErrorName, InterfaceName, MemberName, OwnedUniqueName, WellKnownName};
use zvariant::ObjectPath;

use futures_core::{ready, Future};
use futures_sink::Sink;
use futures_util::{
    future::{select, Either},
    sink::SinkExt,
    StreamExt,
};

use crate::{
    blocking, fdo,
    raw::{Connection as RawConnection, Socket},
    Authenticated, ConnectionBuilder, DBusError, Error, Guid, Message, MessageStream, MessageType,
    ObjectServer, Result,
};

const DEFAULT_MAX_QUEUED: usize = 64;

slotmap::new_key_type! {
    pub(crate) struct SignalHandlerKey;
}

type SignalHandlerHandlerAsyncFunction =
    Box<dyn for<'msg> FnMut(&'msg Arc<Message>) -> BoxFuture<'msg, ()> + Send>;
type DispatchMethodReturnFunction =
    Box<dyn for<'msg> FnOnce(&'msg Arc<Message>) -> BoxFuture<'msg, ()> + Send>;

#[derive(derivative::Derivative)]
#[derivative(Debug)]
pub(crate) struct SignalHandler {
    pub(crate) filter_member: Option<MemberName<'static>>,
    pub(crate) filter_path: Option<ObjectPath<'static>>,
    pub(crate) filter_interface: Option<InterfaceName<'static>>,
    // Note: when fixing issue #69, change this to a match expression object and consider merging
    // it and the filter expressions.
    pub(crate) match_expr: String,
    #[derivative(Debug = "ignore")]
    handler: SignalHandlerHandlerAsyncFunction,
}

impl SignalHandler {
    pub fn signal<H>(
        path: ObjectPath<'static>,
        interface: InterfaceName<'static>,
        member: impl Into<Option<MemberName<'static>>>,
        match_expr: String,
        handler: H,
    ) -> Self
    where
        H: for<'msg> FnMut(&'msg Arc<Message>) -> BoxFuture<'msg, ()> + Send + 'static,
    {
        Self {
            filter_member: member.into(),
            filter_path: Some(path),
            filter_interface: Some(interface),
            match_expr,
            handler: Box::new(handler),
        }
    }
}

/// Inner state shared with tasks that are stopped by ConnectionInner's drop
#[derive(Debug)]
struct ConnectionTaskShared {
    raw_conn: sync::Mutex<RawConnection<Box<dyn Socket>>>,
}

/// Inner state shared by Connection and WeakConnection
#[derive(Debug)]
struct ConnectionInner {
    server_guid: Guid,
    cap_unix_fd: bool,
    bus_conn: bool,
    unique_name: OnceCell<OwnedUniqueName>,
    registered_names: Mutex<HashSet<WellKnownName<'static>>>,

    task_shared: Arc<ConnectionTaskShared>,

    // Serial number for next outgoing message
    serial: AtomicU32,

    // Our executor
    executor: Arc<Executor<'static>>,

    // Message receiver task
    #[allow(unused)]
    msg_receiver_task: Task<()>,

    signal_matches: Mutex<HashMap<String, u64>>,

    object_server: OnceCell<RwLock<blocking::ObjectServer>>,
    object_server_dispatch_task: OnceCell<Task<()>>,
}

/// Inner state that runs synchronized with the flow of incoming messages.
#[derive(derivative::Derivative)]
#[derivative(Debug)]
struct OrderedCallbacks {
    task: OnceCell<Task<()>>,

    // DenseSlotMap is used because we'll likely iterate this more often than modifying it.
    handlers: sync::Mutex<DenseSlotMap<SignalHandlerKey, SignalHandler>>,

    #[derivative(Debug = "ignore")]
    replies: sync::Mutex<HashMap<u32, DispatchMethodReturnFunction>>,
}

// FIXME: Should really use [`AsyncDrop`] for `ConnectionInner` when we've something like that to
//        cancel `msg_receiver_task` manually to ensure task is gone before the connection is. Same
//        goes for the registered well-known names.
//
// [`AsyncDrop`]: https://github.com/rust-lang/wg-async-foundations/issues/65

#[derive(Debug)]
struct MessageReceiverTask {
    task_shared: Arc<ConnectionTaskShared>,

    // Message broadcaster.
    msg_sender: Broadcaster<Arc<Message>>,

    // Sender side of the error channel
    error_sender: Sender<Error>,
}

impl MessageReceiverTask {
    fn new(
        task_shared: Arc<ConnectionTaskShared>,
        msg_sender: Broadcaster<Arc<Message>>,
        error_sender: Sender<Error>,
    ) -> Arc<Self> {
        Arc::new(Self {
            task_shared,
            msg_sender,
            error_sender,
        })
    }

    fn spawn(self: Arc<Self>, executor: &Executor<'_>) -> Task<()> {
        executor.spawn(async move {
            self.receive_msg().await;
        })
    }

    // Keep receiving messages and put them on the queue.
    async fn receive_msg(self: Arc<Self>) {
        loop {
            // Ignore errors from sending to msg or error channels. The only reason these calls
            // fail is when the channel is closed and that will only happen when `Connection` is
            // being dropped.
            // TODO: We should still log in case of error when we've logging.

            let receive_msg = ReceiveMessage {
                raw_conn: &self.task_shared.raw_conn,
            };
            let msg = match receive_msg.await {
                Ok(msg) => msg,
                Err(e) => {
                    // Ignoring errors. See comment above.
                    let _ = self.error_sender.send(e).await;

                    continue;
                }
            };

            let msg = Arc::new(msg);
            // Ignoring errors. See comment above.
            let _ = self.msg_sender.broadcast(msg.clone()).await;
        }
    }
}

impl OrderedCallbacks {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            task: Default::default(),
            handlers: sync::Mutex::new(DenseSlotMap::with_key()),
            replies: sync::Mutex::new(HashMap::new()),
        })
    }

    fn start(self: &Arc<Self>, conn: &Connection) {
        self.task.get_or_init(|| {
            let scope = Arc::downgrade(self);
            let stream = conn.msg_receiver.activate_cloned();
            conn.executor().spawn(Self::run(scope, stream))
        });
    }

    async fn run(weak: Weak<Self>, mut stream: impl Stream<Item = Arc<Message>> + Unpin) {
        while let Some(msg) = stream.next().await {
            let tasks = if let Some(this) = weak.upgrade() {
                this.start_handlers(&msg)
            } else {
                return;
            };
            let () = tasks.collect().await;
        }
    }

    fn start_handlers<'msg>(
        &self,
        msg: &'msg Arc<Message>,
    ) -> FuturesUnordered<BoxFuture<'msg, ()>> {
        let futures = FuturesUnordered::new();
        if msg.message_type() == MessageType::Signal {
            let mut handlers = self.handlers.lock().expect("poisoned lock");
            // TODO if we have lots of handlers, we might want to do smarter filtering
            for (_key, handler) in handlers.iter_mut() {
                if let Some(member) = &handler.filter_member {
                    if msg.member() != Ok(Some(member.as_ref())) {
                        continue;
                    }
                }
                if let Some(path) = &handler.filter_path {
                    if msg.path() != Ok(Some(path.as_ref())) {
                        continue;
                    }
                }
                if let Some(interface) = &handler.filter_interface {
                    if msg.interface() != Ok(Some(interface.as_ref())) {
                        continue;
                    }
                }
                futures.push((handler.handler)(msg));
            }
        } else if let Ok(Some(seq)) = msg.reply_serial() {
            let mut handlers = self.replies.lock().expect("poisoned lock");
            if let Some(handler) = handlers.remove(&seq) {
                futures.push(handler(msg));
            }
        }
        futures
    }
}

/// A D-Bus connection.
///
/// A connection to a D-Bus bus, or a direct peer.
///
/// Once created, the connection is authenticated and negotiated and messages can be sent or
/// received, such as [method calls] or [signals].
///
/// For higher-level message handling (typed functions, introspection, documentation reasons etc),
/// it is recommended to wrap the low-level D-Bus messages into Rust functions with the
/// [`dbus_proxy`] and [`dbus_interface`] macros instead of doing it directly on a `Connection`.
///
/// Typically, a connection is made to the session bus with [`Connection::session`], or to the
/// system bus with [`Connection::system`]. Then the connection is used with [`crate::Proxy`]
/// instances or the on-demand [`ObjectServer`] instance that can be accessed through
/// [`Connection::object_server`] or [`Connection::object_server_mut`].
///
/// `Connection` implements [`Clone`] and cloning it is a very cheap operation, as the underlying
/// data is not cloned. This makes it very convenient to share the connection between different
/// parts of your code. `Connection` also implements [`std::marker::Sync`] and[`std::marker::Send`]
/// so you can send and share a connection instance across threads as well.
///
/// `Connection` keeps an internal queue of incoming message. The maximum capacity of this queue
/// is configurable through the [`set_max_queued`] method. The default size is 64. When the queue is
/// full, no more messages can be received until room is created for more. This is why it's
/// important to ensure that all [`crate::MessageStream`] and [`crate::blocking::MessageStream`]
/// instances are continuously polled and iterated on, respectively.
///
/// For sending messages you can either use [`Connection::send_message`] method or make use of the
/// [`Sink`] implementation. For latter, you might find [`SinkExt`] API very useful. Keep in mind
/// that [`Connection`] will not manage the serial numbers (cookies) on the messages for you when
/// they are sent through the [`Sink`] implementation. You can manually assign unique serial numbers
/// to them using the [`Connection::assign_serial_num`] method before sending them off, if needed.
/// Having said that, the [`Sink`] is mainly useful for sending out signals, as they do not expect
/// a reply, and serial numbers are not very useful for signals either for the same reason.
///
/// Since you do not need exclusive access to a `zbus::Connection` to send messages on the bus,
/// [`Sink`] is also implemented on `&Connection`.
///
/// # Caveats
///
/// At the moment, a simultaneous [flush request] from multiple tasks/threads could
/// potentially create a busy loop, thus wasting CPU time. This limitation may be removed in the
/// future.
///
/// [flush request]: https://docs.rs/futures/0.3.15/futures/sink/trait.SinkExt.html#method.flush
///
/// [method calls]: struct.Connection.html#method.call_method
/// [signals]: struct.Connection.html#method.emit_signal
/// [`dbus_proxy`]: attr.dbus_proxy.html
/// [`dbus_interface`]: attr.dbus_interface.html
/// [`Clone`]: https://doc.rust-lang.org/std/clone/trait.Clone.html
/// [`set_max_queued`]: struct.Connection.html#method.set_max_queued
///
/// ### Examples
///
/// #### Get the session bus ID
///
/// ```
///# use zvariant::Type;
///#
///# async_io::block_on(async {
/// use zbus::Connection;
///
/// let mut connection = Connection::session().await?;
///
/// let reply = connection
///     .call_method(
///         Some("org.freedesktop.DBus"),
///         "/org/freedesktop/DBus",
///         Some("org.freedesktop.DBus"),
///         "GetId",
///         &(),
///     )
///     .await?;
///
/// let id: &str = reply.body()?;
/// println!("Unique ID of the bus: {}", id);
///# Ok::<(), zbus::Error>(())
///# });
/// ```
///
/// #### Monitoring all messages
///
/// Let's eavesdrop on the session bus 😈 using the [Monitor] interface:
///
/// ```rust,no_run
///# async_io::block_on(async {
/// use futures_util::stream::TryStreamExt;
/// use zbus::{Connection, MessageStream};
///
/// let connection = Connection::session().await?;
///
/// connection
///     .call_method(
///         Some("org.freedesktop.DBus"),
///         "/org/freedesktop/DBus",
///         Some("org.freedesktop.DBus.Monitoring"),
///         "BecomeMonitor",
///         &(&[] as &[&str], 0u32),
///     )
///     .await?;
///
/// let mut stream = MessageStream::from(connection);
/// while let Some(msg) = stream.try_next().await? {
///     println!("Got message: {}", msg);
/// }
///
///# Ok::<(), zbus::Error>(())
///# });
/// ```
///
/// This should print something like:
///
/// ```console
/// Got message: Signal NameAcquired from org.freedesktop.DBus
/// Got message: Signal NameLost from org.freedesktop.DBus
/// Got message: Method call GetConnectionUnixProcessID from :1.1324
/// Got message: Error org.freedesktop.DBus.Error.NameHasNoOwner:
///              Could not get PID of name ':1.1332': no such name from org.freedesktop.DBus
/// Got message: Method call AddMatch from :1.918
/// Got message: Method return from org.freedesktop.DBus
/// ```
///
/// [Monitor]: https://dbus.freedesktop.org/doc/dbus-specification.html#bus-messages-become-monitor
#[derive(Clone, Debug)]
pub struct Connection {
    inner: Arc<ConnectionInner>,
    scope: Arc<OrderedCallbacks>,

    pub(crate) msg_receiver: InactiveReceiver<Arc<Message>>,

    // Receiver side of the error channel
    pub(crate) error_receiver: Receiver<Error>,
}

assert_impl_all!(Connection: Send, Sync, Unpin);

impl Connection {
    /// Send `msg` to the peer.
    ///
    /// Unlike our [`Sink`] implementation, this method sets a unique (to this connection) serial
    /// number on the message before sending it off, for you.
    ///
    /// On successfully sending off `msg`, the assigned serial number is returned.
    pub async fn send_message(&self, mut msg: Message) -> Result<u32> {
        let serial = self.assign_serial_num(&mut msg)?;

        (&*self).send(msg).await?;

        Ok(serial)
    }

    /// Send a method call.
    ///
    /// Create a method-call message, send it over the connection, then wait for the reply.
    ///
    /// On successful reply, an `Ok(Message)` is returned. On error, an `Err` is returned. D-Bus
    /// error replies are returned as [`Error::MethodError`].
    pub async fn call_method<'d, 'p, 'i, 'm, D, P, I, M, B>(
        &self,
        destination: Option<D>,
        path: P,
        interface: Option<I>,
        method_name: M,
        body: &B,
    ) -> Result<Arc<Message>>
    where
        D: TryInto<BusName<'d>>,
        P: TryInto<ObjectPath<'p>>,
        I: TryInto<InterfaceName<'i>>,
        M: TryInto<MemberName<'m>>,
        D::Error: Into<Error>,
        P::Error: Into<Error>,
        I::Error: Into<Error>,
        M::Error: Into<Error>,
        B: serde::ser::Serialize + zvariant::DynamicType,
    {
        let m = Message::method(
            self.unique_name(),
            destination,
            path,
            interface,
            method_name,
            body,
        )?;
        self.call_method_raw(m).await
    }

    /// Send a method call.
    ///
    /// Send the given message, which must be a method call, over the connection and wait for the
    /// reply. Typically you'd want to use [`Connection::call_method`] instead.
    ///
    /// On successful reply, an `Ok(Message)` is returned. On error, an `Err` is returned. D-Bus
    /// error replies are returned as [`Error::MethodError`].
    pub async fn call_method_raw(&self, msg: Message) -> Result<Arc<Message>> {
        debug_assert_eq!(msg.message_type(), MessageType::MethodCall);
        let stream = MessageStream::from(self.clone());
        let serial = self.send_message(msg).await?;
        match stream
            .filter(move |m| {
                ready(
                    m.as_ref()
                        .map(|m| {
                            matches!(
                                m.message_type(),
                                MessageType::Error | MessageType::MethodReturn
                            ) && m.reply_serial() == Ok(Some(serial))
                        })
                        .unwrap_or(false),
                )
            })
            .next()
            .await
        {
            Some(msg) => match msg {
                Ok(m) => {
                    match m.message_type() {
                        MessageType::Error => Err(m.into()),
                        MessageType::MethodReturn => Ok(m),
                        // We already established the msg type in `filter` above.
                        _ => unreachable!(),
                    }
                }
                Err(e) => Err(e),
            },
            None => {
                // If SocketStream gives us None, that means the socket was closed
                Err(crate::Error::Io(io::Error::new(
                    ErrorKind::BrokenPipe,
                    "socket closed",
                )))
            }
        }
    }

    /// Send a method call and execute a callback on reply.
    ///
    /// This callback is well-ordered with respect to the other callbacks in this scope; see
    /// [`Connection::new_scope`] for details on why scopes are useful and how to create them.
    /// Having the reply ordered with respect to other callbacks is useful for populating caches
    /// that are updated by signals, which otherwise will contain a race between the return of the
    /// initial population call and an update signal.
    ///
    /// Note: the callback will only be run if the current scope is still alive.
    pub async fn dispatch_call<H>(&self, mut msg: Message, reply: H) -> Result<()>
    where
        H: for<'msg> FnOnce(&'msg Arc<Message>) -> BoxFuture<'msg, ()> + Send + 'static,
    {
        self.scope.start(self);
        let serial = self.assign_serial_num(&mut msg)?;
        self.scope
            .replies
            .lock()
            .expect("poisoned lock")
            .insert(serial, Box::new(reply));

        (&*self).send(msg).await?;

        Ok(())
    }

    /// Emit a signal.
    ///
    /// Create a signal message, and send it over the connection.
    pub async fn emit_signal<'d, 'p, 'i, 'm, D, P, I, M, B>(
        &self,
        destination: Option<D>,
        path: P,
        interface: I,
        signal_name: M,
        body: &B,
    ) -> Result<()>
    where
        D: TryInto<BusName<'d>>,
        P: TryInto<ObjectPath<'p>>,
        I: TryInto<InterfaceName<'i>>,
        M: TryInto<MemberName<'m>>,
        D::Error: Into<Error>,
        P::Error: Into<Error>,
        I::Error: Into<Error>,
        M::Error: Into<Error>,
        B: serde::ser::Serialize + zvariant::DynamicType,
    {
        let m = Message::signal(
            self.unique_name(),
            destination,
            path,
            interface,
            signal_name,
            body,
        )?;

        self.send_message(m).await.map(|_| ())
    }

    /// Reply to a message.
    ///
    /// Given an existing message (likely a method call), send a reply back to the caller with the
    /// given `body`.
    ///
    /// Returns the message serial number.
    pub async fn reply<B>(&self, call: &Message, body: &B) -> Result<u32>
    where
        B: serde::ser::Serialize + zvariant::DynamicType,
    {
        let m = Message::method_reply(self.unique_name(), call, body)?;
        self.send_message(m).await
    }

    /// Reply an error to a message.
    ///
    /// Given an existing message (likely a method call), send an error reply back to the caller
    /// with the given `error_name` and `body`.
    ///
    /// Returns the message serial number.
    pub async fn reply_error<'e, E, B>(
        &self,
        call: &Message,
        error_name: E,
        body: &B,
    ) -> Result<u32>
    where
        B: serde::ser::Serialize + zvariant::DynamicType,
        E: TryInto<ErrorName<'e>>,
        E::Error: Into<Error>,
    {
        let m = Message::method_error(self.unique_name(), call, error_name, body)?;
        self.send_message(m).await
    }

    /// Reply an error to a message.
    ///
    /// Given an existing message (likely a method call), send an error reply back to the caller
    /// using one of the standard interface reply types.
    ///
    /// Returns the message serial number.
    pub async fn reply_dbus_error(
        &self,
        call: &zbus::MessageHeader<'_>,
        err: impl DBusError,
    ) -> Result<u32> {
        let m = err.reply_to(call);
        self.send_message(m?).await
    }

    /// Register a well-known name for this service on the bus.
    ///
    /// You can request multiple names for the same `ObjectServer`. Use [`Connection::release_name`]
    /// for deregistering names registered through this method.
    ///
    /// Note that exclusive ownership without queueing is requested (using
    /// [`fdo::RequestNameFlags::ReplaceExisting`] and [`fdo::RequestNameFlags::DoNotQueue`] flags)
    /// since that is the most typical case. If that is not what you want, you should use
    /// [`fdo::DBusProxy::request_name`] instead (but make sure then that name is requested
    /// **after** you've setup your service implementation with the `ObjectServer`).
    pub async fn request_name<'w, W>(&self, well_known_name: W) -> Result<()>
    where
        W: TryInto<WellKnownName<'w>>,
        W::Error: Into<Error>,
    {
        let well_known_name = well_known_name.try_into().map_err(Into::into)?;
        let mut names = self.inner.registered_names.lock().await;

        if !names.contains(&well_known_name) {
            // Ensure ObjectServer and its msg stream exists and reading before registering any
            // names. Otherwise we get issue#68 (that we warn the user about in the docs of this
            // method).
            self.object_server().await;

            fdo::DBusProxy::new(self)
                .await?
                .request_name(
                    well_known_name.clone(),
                    fdo::RequestNameFlags::ReplaceExisting | fdo::RequestNameFlags::DoNotQueue,
                )
                .await?;
            names.insert(well_known_name.to_owned());
        }

        Ok(())
    }

    /// Deregister a previously registered well-known name for this service on the bus.
    ///
    /// Use this method to deregister a well-known name, registered through
    /// [`Connection::request_name`].
    ///
    /// Unless an error is encountered, returns `Ok(true)` if name was previously registered with
    /// the bus through `self` and it has now been successfully deregistered, `Ok(fasle)` if name
    /// was not previously registered or already deregistered.
    pub async fn release_name<'w, W>(&self, well_known_name: W) -> Result<bool>
    where
        W: TryInto<WellKnownName<'w>>,
        W::Error: Into<Error>,
    {
        let well_known_name: WellKnownName<'w> = well_known_name.try_into().map_err(Into::into)?;
        let mut names = self.inner.registered_names.lock().await;
        // FIXME: Should be possible to avoid cloning/allocation here
        if !names.remove(&well_known_name.to_owned()) {
            return Ok(false);
        }

        fdo::DBusProxy::new(self)
            .await?
            .release_name(well_known_name)
            .await
            .map(|_| true)
            .map_err(Into::into)
    }

    /// Checks if `self` is a connection to a message bus.
    ///
    /// This will return `false` for p2p connections.
    pub fn is_bus(&self) -> bool {
        self.inner.bus_conn
    }

    /// Assigns a serial number to `msg` that is unique to this connection.
    ///
    /// This method can fail if `msg` is corrupt.
    pub fn assign_serial_num(&self, msg: &mut Message) -> Result<u32> {
        let mut serial = 0;
        msg.modify_primary_header(|primary| {
            serial = *primary.serial_num_or_init(|| self.next_serial());
            Ok(())
        })?;

        Ok(serial)
    }

    /// The unique name as assigned by the message bus or `None` if not a message bus connection.
    pub fn unique_name(&self) -> Option<&OwnedUniqueName> {
        self.inner.unique_name.get()
    }

    /// Max number of messages to queue.
    pub fn max_queued(&self) -> usize {
        self.msg_receiver.capacity()
    }

    /// Set the max number of messages to queue.
    pub fn set_max_queued(&mut self, max: usize) {
        self.msg_receiver.set_capacity(max);
    }

    /// The server's GUID.
    pub fn server_guid(&self) -> &str {
        self.inner.server_guid.as_str()
    }

    /// The underlying executor.
    ///
    /// When a connection is built with internal_executor set to false, zbus will not spawn a
    /// thread to run the executor. You're responsible to continuously [tick the executor][tte].
    /// Failure to do so will result in hangs.
    ///
    /// # Examples
    ///
    /// Here is how one would typically run the zbus executor through tokio's single-threaded
    /// scheduler:
    ///
    /// ```
    /// use zbus::ConnectionBuilder;
    /// use tokio::runtime;
    ///
    /// runtime::Builder::new_current_thread()
    ///        .build()
    ///        .unwrap()
    ///        .block_on(async {
    ///     let conn = ConnectionBuilder::session()
    ///         .unwrap()
    ///         .internal_executor(false)
    ///         .build()
    ///         .await
    ///         .unwrap();
    ///     {
    ///        let conn = conn.clone();
    ///        tokio::task::spawn(async move {
    ///            loop {
    ///                conn.executor().tick().await;
    ///            }
    ///        });
    ///     }
    ///
    ///     // All your other async code goes here.
    /// });
    /// ```
    ///
    /// [tte]: https://docs.rs/async-executor/1.4.1/async_executor/struct.Executor.html#method.tick
    pub fn executor(&self) -> &Executor<'static> {
        &self.inner.executor
    }

    /// Get a reference to the associated [`ObjectServer`].
    ///
    /// The `ObjectServer` is created on-demand.
    pub async fn object_server(&self) -> impl Deref<Target = ObjectServer> + '_ {
        // FIXME: Maybe it makes sense after all to implement Deref<Target= ObjectServer> for
        // crate::ObjectServer instead of this wrapper?
        struct Wrapper<'s>(RwLockReadGuard<'s, blocking::ObjectServer>);
        impl Deref for Wrapper<'_> {
            type Target = ObjectServer;

            fn deref(&self) -> &Self::Target {
                self.0.inner()
            }
        }

        Wrapper(self.sync_object_server().await)
    }

    pub(crate) async fn sync_object_server(&self) -> RwLockReadGuard<'_, blocking::ObjectServer> {
        self.inner
            .object_server
            .get_or_init(|| self.setup_object_server())
            .read()
            .await
    }

    /// Get a mutable reference to the associated [`ObjectServer`].
    ///
    /// The `ObjectServer` is created on-demand.
    ///
    /// # Caveats
    ///
    /// The return value of this method should not be kept around for longer than needed. The method
    /// dispatch machinery of the [`ObjectServer`] will be paused as long as the return value is alive.
    pub async fn object_server_mut(&self) -> impl DerefMut<Target = ObjectServer> + '_ {
        // FIXME: Maybe it makes sense after all to implement DerefMut<Target= ObjectServer>
        // for crate::ObjectServer instead of this wrapper?
        struct Wrapper<'s>(RwLockWriteGuard<'s, blocking::ObjectServer>);
        impl Deref for Wrapper<'_> {
            type Target = ObjectServer;

            fn deref(&self) -> &Self::Target {
                self.0.inner()
            }
        }
        impl DerefMut for Wrapper<'_> {
            fn deref_mut(&mut self) -> &mut Self::Target {
                self.0.inner_mut()
            }
        }

        Wrapper(self.sync_object_server_mut().await)
    }

    pub(crate) async fn sync_object_server_mut(
        &self,
    ) -> RwLockWriteGuard<'_, blocking::ObjectServer> {
        self.inner
            .object_server
            .get_or_init(|| self.setup_object_server())
            .write()
            .await
    }

    fn setup_object_server(&self) -> RwLock<blocking::ObjectServer> {
        if self.is_bus() {
            self.start_object_server();
        }

        RwLock::new(blocking::ObjectServer::new(self))
    }

    pub(crate) fn start_object_server(&self) {
        self.inner.object_server_dispatch_task.get_or_init(|| {
            let weak_conn = WeakConnection::from(self);
            let mut stream = MessageStream::from(self.clone());

            self.inner.executor.spawn(async move {
                // TODO: Log errors when we've logging.
                while let Some(msg) = stream.next().await.and_then(|m| m.ok()) {
                    if let Some(conn) = weak_conn.upgrade() {
                        let executor = conn.inner.executor.clone();
                        executor
                            .spawn(async move {
                                let server = conn.object_server().await;
                                let _ = server.dispatch_message(&msg).await;
                            })
                            .detach();
                    } else {
                        // If connection is completely gone, no reason to keep running the task anymore.
                        break;
                    }
                }
            })
        });
    }

    pub(crate) async fn add_signal_handler(
        &self,
        handler: SignalHandler,
    ) -> Result<SignalHandlerKey> {
        self.scope.start(self);
        if self.is_bus() {
            self.add_match(handler.match_expr.clone()).await?;
        }
        Ok(self
            .scope
            .handlers
            .lock()
            .expect("poisoned lock")
            .insert(handler))
    }

    pub(crate) fn queue_remove_signal_handler(&self, key: SignalHandlerKey) {
        let conn = self.clone();
        self.inner
            .executor
            .spawn(async move { conn.remove_signal_handler(key).await })
            .detach()
    }

    pub(crate) async fn remove_signal_handler(&self, key: SignalHandlerKey) -> Result<bool> {
        let handler = self
            .scope
            .handlers
            .lock()
            .expect("poisoned lock")
            .remove(key);
        match handler {
            Some(h) => {
                if self.is_bus() {
                    self.remove_match(h.match_expr).await?;
                }
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn add_match(&self, expr: String) -> Result<()> {
        use std::collections::hash_map::Entry;
        let mut subscriptions = self.inner.signal_matches.lock().await;
        match subscriptions.entry(expr) {
            Entry::Vacant(e) => {
                fdo::DBusProxy::builder(self)
                    .cache_properties(false)
                    .build()
                    .await?
                    .add_match(e.key())
                    .await?;
                e.insert(1);
            }
            Entry::Occupied(mut e) => {
                *e.get_mut() += 1;
            }
        }
        Ok(())
    }

    async fn remove_match(&self, expr: String) -> Result<bool> {
        use std::collections::hash_map::Entry;
        let mut subscriptions = self.inner.signal_matches.lock().await;
        // TODO when it becomes stable, use HashMap::raw_entry and only require expr: &str
        // (both here and in add_match)
        match subscriptions.entry(expr) {
            Entry::Vacant(_) => Ok(false),
            Entry::Occupied(mut e) => {
                *e.get_mut() -= 1;
                if *e.get() == 0 {
                    fdo::DBusProxy::builder(self)
                        .cache_properties(false)
                        .build()
                        .await?
                        .remove_match(e.key())
                        .await?;
                    e.remove();
                }
                Ok(true)
            }
        }
    }

    /// Create a new callback ordering scope.
    ///
    /// Note: this is an advanced feature that is not normally needed.  It only applies if you use
    /// callbacks (the `connect_*` or `dispatch_*` APIs) and not just the `Stream`/`Future` API.
    ///
    /// Connections support callbacks for executing user-defined functions in response to incoming
    /// messages.  Callbacks may be added using [`crate::Proxy::connect_signal`] or the wrappers
    /// generated by the `dbus_proxy` macro.  By default, there is a single scope for callbacks
    /// shared by all clones of a single Connection.
    ///
    /// Within a scope, all callbacks for a given [`Message`] are completed before the next message
    /// is handled.  If these callbacks are long-running or need to make further dbus calls, this
    /// could cause delays in starting other callbacks.
    ///
    /// This function creates a new Connection clone with independent scope for ordering its
    /// callbacks.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use std::collections::HashMap;
    /// use std::sync::{Arc,Mutex};
    /// use zbus::Connection;
    /// # struct SDB;
    /// # type StudentRecord = ();
    /// # impl SDB {
    /// #   async fn lookup(&self, s: &String) -> StudentRecord {}
    /// # }
    /// # static student_db : SDB = SDB;
    /// #
    /// #[zbus::dbus_proxy]
    /// trait Enroll {
    ///     #[dbus_proxy(signal)]
    ///     fn enroll(&self, names: Vec<String>);
    /// }
    ///
    /// # type Announcement = ();
    /// # type PingMessage = ();
    /// # trait Mock {
    /// #    fn words_mut(&mut self) -> Vec<()> { vec![] }
    /// #    fn str(&self) -> &str { "" }
    /// #    fn add_link(&self, record: &StudentRecord) {}
    /// #    fn format(&self) -> &str { "" }
    /// # }
    /// # impl Mock for () {}
    /// #[zbus::dbus_proxy]
    /// trait Announce {
    ///     #[dbus_proxy(signal)]
    ///     fn announce(&self, msg: Announcement);
    /// }
    ///
    /// #[zbus::dbus_proxy]
    /// trait Baz {
    ///     #[dbus_proxy(property)]
    ///     fn location(&self) -> zbus::fdo::Result<i32>;
    ///     #[dbus_proxy(signal)]
    ///     fn urgent_ping(&self, msg: PingMessage);
    /// }
    ///
    /// # async_io::block_on(async {
    /// let names : Arc<Mutex<HashMap<String,StudentRecord>>> = Default::default();
    ///
    /// let conn = Connection::session().await?;
    ///
    /// let enroll = EnrollProxy::new(&conn).await?;
    /// let daily_announcements = AnnounceProxy::new(&conn).await?;
    /// let conn2 = conn.new_scope();
    /// let baz = BazProxy::new(&conn2).await?;
    ///
    /// let names1 = names.clone();
    /// enroll.connect_enroll(move |students| {
    ///     let names = names1.clone();
    ///     Box::pin(async move {
    ///         for student in students {
    ///             let record = student_db.lookup(&student).await; // this might take a while
    ///             names.lock().unwrap().insert(student, record);
    ///         }
    ///     })
    /// });
    /// // We need to add hyperlinks to any student name before displaying the message.  A message
    /// // will often be sent just after enrollment, so we must be sure that processing is done
    /// // in order, even if it delays the announcement a bit.
    /// let names1 = names.clone();
    /// daily_announcements.connect_announce(move |mut announcement| {
    ///     let names = names1.clone();
    ///     Box::pin(async move {
    ///         let names = names.lock().unwrap();
    ///         for word in announcement.words_mut() {
    ///             if let Some(record) = names.get(word.str()) {
    ///                 word.add_link(&record);
    ///             }
    ///         }
    ///         println!("Global announcement: {}", announcement.format());
    ///     })
    /// });
    ///
    /// baz.connect_location_changed(|pos| Box::pin(async move {
    ///     println!("Baz has moved to {:?}", pos);
    /// }));
    /// baz.connect_urgent_ping(|msg| Box::pin(async move {
    ///     println!("Baz has an urgent message for you: {}", msg.format());
    /// }));
    /// // If two baz signals arrive at the same time, we need to be sure to print them in the
    /// // right order, or the message will appear to come from the wrong location.
    ///
    ///# Ok::<(), zbus::Error>(())
    ///# });
    /// ```
    ///
    /// The `connect_` API lets you be sure you handle signals in order and completely handle one
    /// signal before starting on the next; this is required because it would be an error to handle
    /// either the announcement or ping messages out of order.  However, the baz interface is
    /// completely unrelated to yaks and shouldn't delay urgent messages just because you are busy
    /// shaving.  Placing the baz callbacks in a distinct scope from the foo/bar ones allows them
    /// to be handled in parallel, resulting in minimal or no delays in their output.
    pub fn new_scope(&self) -> Connection {
        let mut rv = self.clone();
        rv.scope = OrderedCallbacks::new();
        rv
    }

    async fn hello_bus(&self) -> Result<()> {
        let dbus_proxy = fdo::DBusProxy::builder(self)
            .cache_properties(false)
            .build()
            .await?;
        let future = dbus_proxy.hello();

        // With external executor, our executor is only run after the connection construction is
        // completed and this method is (and must) run before that so we need to tick the executor
        // ourselves in parallel to making the method call.  With the internal executor, this is
        // not needed but harmless.
        let name = {
            let executor = self.inner.executor.clone();
            let ticking_future = async move {
                // Keep running as long as this task/future is not cancelled.
                loop {
                    executor.tick().await;
                }
            };

            futures_util::pin_mut!(future);
            futures_util::pin_mut!(ticking_future);

            match select(future, ticking_future).await {
                Either::Left((res, _)) => res?,
                Either::Right((_, _)) => unreachable!("ticking task future shouldn't finish"),
            }
        };

        self.inner
            .unique_name
            .set(name)
            // programmer (probably our) error if this fails.
            .expect("Attempted to set unique_name twice");

        Ok(())
    }

    pub(crate) async fn new(
        auth: Authenticated<Box<dyn Socket>>,
        bus_connection: bool,
        internal_executor: bool,
    ) -> Result<Self> {
        let auth = auth.into_inner();
        let cap_unix_fd = auth.cap_unix_fd;

        let (msg_sender, msg_receiver) = broadcast(DEFAULT_MAX_QUEUED);
        let msg_receiver = msg_receiver.deactivate();
        let (error_sender, error_receiver) = bounded(1);
        let executor = Arc::new(Executor::new());
        let task_shared = Arc::new(ConnectionTaskShared {
            raw_conn: sync::Mutex::new(auth.conn),
        });
        let scope = OrderedCallbacks::new();

        // Start the message receiver task.
        let msg_receiver_task =
            MessageReceiverTask::new(task_shared.clone(), msg_sender, error_sender)
                .spawn(&executor);

        let connection = Self {
            error_receiver,
            msg_receiver,
            scope,
            inner: Arc::new(ConnectionInner {
                task_shared,
                server_guid: auth.server_guid,
                cap_unix_fd,
                bus_conn: bus_connection,
                serial: AtomicU32::new(1),
                unique_name: OnceCell::new(),
                signal_matches: Mutex::new(HashMap::new()),
                object_server: OnceCell::new(),
                object_server_dispatch_task: OnceCell::new(),
                executor: executor.clone(),
                msg_receiver_task,
                registered_names: Mutex::new(HashSet::new()),
            }),
        };

        if internal_executor {
            std::thread::Builder::new()
                .name("zbus::Connection executor".into())
                .spawn(move || {
                    block_on(async move {
                        // Run as long as there is a task to run.
                        while !executor.is_empty() {
                            executor.tick().await;
                        }
                    })
                })?;
        }

        if !bus_connection {
            return Ok(connection);
        }

        // Now that the server has approved us, we must send the bus Hello, as per specs
        connection.hello_bus().await?;

        Ok(connection)
    }

    fn next_serial(&self) -> u32 {
        self.inner.serial.fetch_add(1, SeqCst)
    }

    /// Create a `Connection` to the session/user message bus.
    pub async fn session() -> Result<Self> {
        ConnectionBuilder::session()?.build().await
    }

    /// Create a `Connection` to the system-wide message bus.
    pub async fn system() -> Result<Self> {
        ConnectionBuilder::system()?.build().await
    }

    /// Returns a listener, notified on various connection activity.
    ///
    /// This function is meant for the caller to implement idle or timeout on inactivity.
    pub fn monitor_activity(&self) -> EventListener {
        self.inner
            .task_shared
            .raw_conn
            .lock()
            .expect("poisoned lock")
            .monitor_activity()
    }
}

impl Sink<Message> for Connection {
    type Error = Error;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        Pin::new(&mut &*self).poll_ready(cx)
    }

    fn start_send(self: Pin<&mut Self>, msg: Message) -> Result<()> {
        Pin::new(&mut &*self).start_send(msg)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        Pin::new(&mut &*self).poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        Pin::new(&mut &*self).poll_close(cx)
    }
}

impl<'a> Sink<Message> for &'a Connection {
    type Error = Error;

    fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<()>> {
        // TODO: We should have a max queue length in raw::Socket for outgoing messages.
        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, msg: Message) -> Result<()> {
        if !msg.fds().is_empty() && !self.inner.cap_unix_fd {
            return Err(Error::Unsupported);
        }

        self.inner
            .task_shared
            .raw_conn
            .lock()
            .expect("poisoned lock")
            .enqueue_message(msg);

        Ok(())
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        self.inner
            .task_shared
            .raw_conn
            .lock()
            .expect("poisoned lock")
            .flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        let mut raw_conn = self
            .inner
            .task_shared
            .raw_conn
            .lock()
            .expect("poisoned lock");
        match ready!(raw_conn.flush(cx)) {
            Ok(_) => (),
            Err(e) => return Poll::Ready(Err(e)),
        }

        Poll::Ready(raw_conn.close())
    }
}

struct ReceiveMessage<'r> {
    raw_conn: &'r sync::Mutex<RawConnection<Box<dyn Socket>>>,
}

impl<'r> Future for ReceiveMessage<'r> {
    type Output = Result<Message>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut raw_conn = self.raw_conn.lock().expect("poisoned lock");
        raw_conn.try_receive_message(cx)
    }
}

impl From<crate::blocking::Connection> for Connection {
    fn from(conn: crate::blocking::Connection) -> Self {
        conn.into_inner()
    }
}

// Internal API that allows keeping a weak connection ref around.
#[derive(Debug)]
pub(crate) struct WeakConnection {
    inner: Weak<ConnectionInner>,
    // This does not need to be weak because it does not cause a cyclic reference. It may also be
    // the only remaining reference to the original scope (and ObjectServer needs to have a scope
    // available for its callbacks in case they add signal handlers).
    scope: Arc<OrderedCallbacks>,
    msg_receiver: InactiveReceiver<Arc<Message>>,
    error_receiver: Receiver<Error>,
}

impl WeakConnection {
    /// Upgrade to a Connection.
    pub fn upgrade(&self) -> Option<Connection> {
        self.inner.upgrade().map(|inner| Connection {
            inner,
            scope: self.scope.clone(),
            msg_receiver: self.msg_receiver.clone(),
            error_receiver: self.error_receiver.clone(),
        })
    }
}

impl From<&Connection> for WeakConnection {
    fn from(conn: &Connection) -> Self {
        Self {
            inner: Arc::downgrade(&conn.inner),
            scope: conn.scope.clone(),
            msg_receiver: conn.msg_receiver.clone(),
            error_receiver: conn.error_receiver.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use futures_util::stream::TryStreamExt;
    use ntest::timeout;
    use std::os::unix::net::UnixStream;
    use test_env_log::test;

    use super::*;

    #[test]
    #[timeout(15000)]
    fn unix_p2p() {
        async_io::block_on(test_unix_p2p()).unwrap();
    }

    async fn test_unix_p2p() -> Result<()> {
        let guid = Guid::generate();

        let (p0, p1) = UnixStream::pair().unwrap();

        let server = ConnectionBuilder::unix_stream(p0)
            .server(&guid)
            .p2p()
            .build();
        let client = ConnectionBuilder::unix_stream(p1).p2p().build();

        let (client_conn, server_conn) = futures_util::try_join!(client, server)?;

        let server_future = async {
            let mut method: Option<Arc<Message>> = None;
            let mut stream = MessageStream::from(&server_conn);
            while let Some(m) = stream.try_next().await? {
                if m.to_string() == "Method call Test" {
                    method.replace(m);

                    break;
                }
            }
            let method = method.unwrap();

            // Send another message first to check the queueing function on client side.
            server_conn
                .emit_signal(None::<()>, "/", "org.zbus.p2p", "ASignalForYou", &())
                .await?;
            server_conn.reply(&method, &("yay")).await
        };

        let client_future = async {
            let mut stream = MessageStream::from(&client_conn);
            let reply = client_conn
                .call_method(None::<()>, "/", Some("org.zbus.p2p"), "Test", &())
                .await?;
            assert_eq!(reply.to_string(), "Method return");
            // Check we didn't miss the signal that was sent during the call.
            let m = stream.try_next().await?.unwrap();
            assert_eq!(m.to_string(), "Signal ASignalForYou");
            reply.body::<String>()
        };

        let (val, _) = futures_util::try_join!(client_future, server_future)?;
        assert_eq!(val, "yay");

        Ok(())
    }

    #[test]
    #[timeout(15000)]
    fn serial_monotonically_increases() {
        async_io::block_on(test_serial_monotonically_increases());
    }

    async fn test_serial_monotonically_increases() {
        let c = Connection::session().await.unwrap();
        let serial = c.next_serial() + 1;

        for next in serial..serial + 10 {
            assert_eq!(next, c.next_serial());
        }
    }
}
