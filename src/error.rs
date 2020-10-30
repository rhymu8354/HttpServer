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

    /// The raw request received from the client could not be parsed.
    #[error("unable to parse HTTP request")]
    BadRequest(#[source] rhymuweb::Error),

    /// The raw response to be sent to the client could not be turned into a
    /// valid byte stream.
    #[error("unable to generate HTTP response")]
    BadResponse(#[source] rhymuweb::Error),

    /// The server could not bind or listen to the transport layer port.
    #[error("unable to bind or listen to the transport layer port")]
    Bind(#[source] std::io::Error),

    /// The connection to the client was lost while awaiting a request.
    #[error("disconnected from client")]
    Disconnected,

    /// The server encountered an error attempting to receive the
    /// request from the client.
    #[error("unable to receive request from client")]
    UnableToReceive(#[source] std::io::Error),

    /// The server encountered an error attempting to send the
    /// response to the client.
    #[error("unable to send response to client")]
    UnableToSend(#[source] std::io::Error),
}
