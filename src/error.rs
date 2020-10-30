/// This is the enumeration of all the different kinds of errors which this
/// crate generates.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The server cannot be started because it's already running.  You'll need
    /// to stop the server first by calling
    /// [`stop`](struct.HttpServer.html#method.stop).
    ///
    /// TODO: We probably don't need this error, but let's keep it just in
    /// case.
    #[error("cannot start the server because it's already running")]
    AlreadyStarted,

    /// The server cannot be stopped because it's not running.
    #[error("cannot stop the server because it's not running")]
    AlreadyStopped,

    /// The server could not bind or listen to the transport layer port.
    #[error("unable to bind or listen to the transport layer port")]
    Bind(#[source] std::io::Error),
}
