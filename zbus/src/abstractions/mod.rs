/// This mod contains a bunch of abstractions.
///
/// These abstractions allow us to make use of the appropriate API depending on which features are
/// enabled.
mod executor;
pub use executor::*;
pub(crate) mod async_lock;
