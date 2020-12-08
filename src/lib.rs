#![warn(clippy::pedantic)]
// TODO: Remove this once ready to publish.
#![allow(clippy::missing_errors_doc)]
// This warning falsely triggers when `future::select_all` is used.
#![allow(clippy::mut_mut)]
// TODO: Uncomment this once ready to publish.
//#![warn(missing_docs)]

mod error;

use async_std::net::{
    Ipv4Addr,
    TcpListener,
    TcpStream,
};
pub use error::{
    ConnectionWrapper as ConnectionWrapperError,
    Error,
};
use futures::{
    channel::{
        mpsc,
        oneshot,
    },
    executor,
    future,
    AsyncRead,
    AsyncReadExt,
    AsyncWrite,
    AsyncWriteExt,
    FutureExt,
    StreamExt,
};
use rhymuweb::{
    Request,
    RequestParseStatus,
    Response,
};
use std::{
    cell::RefCell,
    collections::HashMap,
    error::Error as _,
    future::Future,
    net::SocketAddr,
    pin::Pin,
    sync::{
        Arc,
        Mutex,
    },
    thread,
};

pub trait Connection: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> Connection for T {}

pub type OnUpgradedCallback = dyn FnOnce(Box<dyn Connection>) + Send;

pub type ConnectionWrapperResult =
    Result<Box<dyn Connection>, Box<ConnectionWrapperError>>;

pub type ConnectionWrapFuture =
    Pin<Box<dyn Future<Output = ConnectionWrapperResult> + Send + 'static>>;

pub type ConnectionWrapper =
    dyn Fn(Box<dyn Connection>) -> ConnectionWrapFuture + Send + 'static;

pub struct FetchResults {
    pub response: Response,
    pub connection: Box<dyn Connection>,
    pub on_upgraded: Option<Box<OnUpgradedCallback>>,
}

pub type ResourceFuture = Pin<Box<dyn Future<Output = FetchResults> + Send>>;

type ResourceHandler =
    dyn Fn(Request, Box<dyn Connection>) -> ResourceFuture + Send + Sync;

type ResourceHandlerCollection = HashMap<Vec<Vec<u8>>, Arc<ResourceHandler>>;

enum WorkerMessage {
    // This tells the worker thread to terminate.
    Exit,

    // This tells the worker thread to start listening for incoming requests
    // from HTTP clients.
    StartListening {
        listener: TcpListener,
        result_sender: oneshot::Sender<Result<(), Error>>,
        connection_wrapper: Box<ConnectionWrapper>,
    },

    RegisterHandler {
        path: Vec<Vec<u8>>,
        handler: Arc<ResourceHandler>,
    },
}

enum ListenerMessage {
    Start {
        listener: TcpListener,
        connection_wrapper: Box<ConnectionWrapper>,
    },
    // Stop,
}

async fn accept_connection(
    connection: TcpStream,
    address: SocketAddr,
    connection_wrapper: &RefCell<Option<Box<ConnectionWrapper>>>,
    connection_sender: &mpsc::UnboundedSender<ConnectionInfo>,
    server_info: &str,
) -> Result<(), Error> {
    let mut connection: Box<dyn Connection> = Box::new(connection);
    // NOTE: `Option::take` is called directly rather than calling `take`
    // on the option, in order to avoid a false error detection by
    // rust-analyzer.  Try going back to calling `take` on the option
    // once whatever bug is in rust-analyzer is fixed.
    let current_connection_wrapper =
        Option::take(&mut connection_wrapper.borrow_mut());
    if let Some(current_connection_wrapper) = current_connection_wrapper {
        let connection_wrapping_result =
            current_connection_wrapper(connection).await;
        connection_wrapper.replace(Some(current_connection_wrapper));
        connection = connection_wrapping_result
            .map_err(|error| Error::ConnectionWrapper(error))?;
    }
    // This should never fail because the connection receiver
    // never completes.  Each connection will be at least
    // accepted by the processor future constructed to
    // handle it.
    connection_sender
        .unbounded_send(ConnectionInfo {
            address,
            connection,
            server_info: String::from(server_info),
        })
        .expect("connection dropped before it reached a processor");
    Ok(())
}

async fn accept_connections(
    mut listener_receiver: mpsc::UnboundedReceiver<ListenerMessage>,
    connection_sender: mpsc::UnboundedSender<ConnectionInfo>,
    server_info: &str,
) {
    // Listener is in a `RefCell` because two separate futures need to access
    // it:
    // * `accepter` takes the listener, uses it, and potentially puts it back.
    // * `message_handler` places new listeners there (potentially dropping any
    //   previous listeners that might still be there).
    let listener: RefCell<Option<TcpListener>> = RefCell::new(None);
    let connection_wrapper: RefCell<Option<Box<ConnectionWrapper>>> =
        RefCell::new(None);
    loop {
        let accepter = async {
            // This future should only do something and complete if we have
            // a listener.  If we don't, we need the other future to complete,
            // and we have nothing to do here, so never complete.
            let current_listener = listener.borrow_mut().take();
            if let Some(current_listener) = current_listener {
                // Here we wait on the listener for the next incoming
                // connection from an HTTP client.  If we get a connection, we
                // need to put the listener back so that when we loop back and
                // "run this future again" (technically a new future is made
                // with this same async block) we can take the listener back
                // out.
                if let Ok((connection, address)) =
                    current_listener.accept().await
                {
                    if listener.borrow().is_none() {
                        listener.replace(Some(current_listener));
                    }
                    println!(
                        "accept_connections: Connection received from {}",
                        address
                    );
                    if let Err(error) = accept_connection(
                        connection,
                        address,
                        &connection_wrapper,
                        &connection_sender,
                        server_info,
                    )
                    .await
                    {
                        match error.source() {
                            Some(source) => eprintln!(
                                "{}: error: {} ({})",
                                address, error, source
                            ),
                            None => eprintln!("{}: error: {}", address, error),
                        };
                    }
                } else {
                    // TODO: This happens if the listener breaks somehow.
                    // We should set up a mechanism for reporting this.
                }
            } else {
                // This is a fancy nerdy way of saying the following:
                // "So, little future, guess what.  You're never going
                // to escape, so just sleep here forever!"
                futures::future::pending().await
            }
        };
        let message_handler = async {
            // If `listener_receiver` completes (has no next), it means the
            // sender end, which is held by `handle_messages`, was dropped
            // somehow.  This can only really happen if there are no more
            // listeners to receive and the `handle_messages` future has
            // completed, which only happens if the worker thread overall is
            // told to exit.  In that case, we don't want to loop trying to get
            // the next message here, because it would either panic or
            // constantly
            match listener_receiver.next().await {
                Some(ListenerMessage::Start {
                    listener: new_listener,
                    connection_wrapper: new_connection_wrapper,
                }) => {
                    println!(
                        "accept_connections: Now accepting connections on port {}",
                        new_listener.local_addr().expect(
                            "we should have been able to figure out our own address"
                        ).port()
                    );
                    listener.replace(Some(new_listener));
                    connection_wrapper.replace(Some(new_connection_wrapper));
                },
                None => futures::future::pending().await,
            }
        };
        futures::select!(
            () = accepter.fuse() => {},
            () = message_handler.fuse() => {},
        );
    }
}

struct ConnectionInfo {
    address: SocketAddr,
    connection: Box<dyn Connection>,
    server_info: String,
}

// This is the type of value returned by a completed future selected by
// `accept_connections`.  It's used to tell the difference between a future
// which processed a connection from a future which processed the connection
// receiver.
enum ProcessorKind {
    Connection,
    Receiver {
        receiver: mpsc::UnboundedReceiver<ConnectionInfo>,
        connection_info: ConnectionInfo,
    },
}

async fn receive_request(
    connection: &mut dyn Connection,
    mut receive_buffer: Vec<u8>,
) -> Result<(Request, Vec<u8>), Error> {
    let mut request = Request::new();
    loop {
        let left_over = receive_buffer.len();
        receive_buffer.resize(left_over + 65536, 0);
        let received = connection
            .read(&mut receive_buffer[left_over..])
            .await
            .map_err(Error::UnableToReceive)
            .and_then(|received| match received {
                0 => Err(Error::Disconnected),
                received => Ok(received),
            })?;
        receive_buffer.truncate(left_over + received);
        let request_status =
            request.parse(&mut receive_buffer).map_err(Error::BadRequest)?;
        receive_buffer.drain(0..request_status.consumed);
        if request_status.status == RequestParseStatus::Complete {
            return Ok((request, receive_buffer));
        }
    }
}

async fn handle_connection(
    connection_info: ConnectionInfo,
    handlers: Arc<Mutex<ResourceHandlerCollection>>,
) -> Result<(), Error> {
    let ConnectionInfo {
        address,
        connection,
        server_info,
    } = connection_info;
    let mut connection_origin = Some(connection);
    let mut left_overs = Some(Vec::new());
    loop {
        // Assemble HTTP request from incoming data.
        let mut connection = connection_origin
            .take()
            .expect("we somehow dropped the connection");
        let (request, trailer) = receive_request(
            &mut connection,
            left_overs
                .take()
                .expect("we somehow dropped the left-overs buffer"),
        )
        .await?;

        // Peek into the request to see if we should close this connection
        // after the response has been sent.
        let close_after_response =
            request.headers.has_header_token("Connection", "close");

        // Dispatch request to handler (use default "not
        // found" handler if we can't find one) to produce an HTTP
        // response.
        let request_id = format!("{} {}", request.method, request.target);
        let handler_factory_reference = handlers
            .lock()
            .expect("")
            .get(request.target.path())
            .map(Clone::clone);
        let (mut response, mut connection, on_upgraded) =
            if let Some(handler_factory) = handler_factory_reference {
                let handler = handler_factory(request, connection);
                let fetch_results = handler.await;
                (
                    fetch_results.response,
                    fetch_results.connection,
                    fetch_results.on_upgraded,
                )
            } else {
                let mut response = Response::new();
                response.status_code = 404;
                response.reason_phrase = "Not Found".into();
                (response, connection, None)
            };
        println!(
            "{}: {} - {} {} ({} bytes)",
            address,
            request_id,
            response.status_code,
            response.reason_phrase,
            response.body.len()
        );

        // Add standard headers.
        response.headers.set_header("Server", &server_info);

        // Set `Content-Length` header if there is no transfer encoding, as
        // long as there is a body or the connection will be reused for another
        // request.
        if !response.headers.has_header("Transfer-Encoding")
            && (!response.body.is_empty()
                || (!close_after_response && response.status_code != 101))
        {
            response
                .headers
                .set_header("Content-Length", response.body.len().to_string());
        }

        // If we're supposed to close the connection after sending
        // the response, tell the client so via the "close" token
        // in the "Connection" header.
        if close_after_response {
            let mut tokens = response.headers.header_tokens("Connection");
            if tokens.iter().all(|token| token != "close") {
                tokens.push("close".into());
                response.headers.set_header("Connection", tokens.join(", "));
            }
        }

        // Send the HTTP response back through the connection.
        let raw_response = response.generate().map_err(Error::BadResponse)?;
        connection
            .write_all(&raw_response)
            .await
            .map_err(Error::UnableToSend)?;

        // If we're supposed to close the connection after sending
        // the response, hold onto it for an arbitrary but relatively
        // short amount of time, to ensure the entire response makes
        // it through the protocol stack and transmitted to the client.
        //
        // TODO: It might not actually necessary to do this.  It all
        // depends on how the protocol stack, operating system, and
        // Rust libraries handle the socket and any data associated
        // with it that the client still hasn't received at the moment
        // the underlying `TcpStream` is dropped.
        //
        // A compromise might be to close the connection if either of the
        // following happens first:
        //
        // 1. The client closes their end (we get a read output of 0 bytes,
        //    in other words, EOF).
        // 2. Some arbitrary grace period elapses (like 5 seconds).
        //
        // Otherwise, put the connection back so it can be reused for the
        // next request at the top of the loop.
        if close_after_response {
            async_std::future::timeout(
                std::time::Duration::from_secs(5),
                futures::future::pending::<()>(),
            )
            .await
            .unwrap_or(());
            return Ok(());
        } else if response.status_code == 101 {
            if let Some(on_upgraded) = on_upgraded {
                on_upgraded(connection);
            }
            return Ok(());
        } else {
            connection_origin.replace(connection);
            left_overs.replace(trailer);
        }
    }
}

async fn await_next_connection(
    mut receiver: mpsc::UnboundedReceiver<ConnectionInfo>
) -> ProcessorKind {
    // Wait for the next connection to come in.
    //
    // If `receiver` completes (has no next), it means
    // the sender end, which is held by `accept_connections`, was
    // dropped somehow.  This should never happen, since that
    // future never completes.
    let next_connection = receiver.next();
    let connection_info = next_connection.await.expect(
        "this task receives connections from other task which should never complete"
    );

    // The output indicates that this is the future used
    // to receive the next connection.
    ProcessorKind::Receiver {
        receiver,
        connection_info,
    }
}

async fn handle_connections(
    connection_receiver: mpsc::UnboundedReceiver<ConnectionInfo>,
    handlers: Arc<Mutex<ResourceHandlerCollection>>,
) {
    let mut processors = Vec::new();
    let mut needs_next_connection = true;

    // We need to wrap `connection_receiver` in an Option because we need to
    // temporarily give it to a future (`handle_next_connection`) and receive
    // `connection_receiver` back when that future completes.
    let mut connection_receiver = Some(connection_receiver);
    loop {
        // Add a special future to receive the next connection.
        if needs_next_connection {
            // `connection_receiver` should always be Some(connection)
            // because we initialize it that way, and every time
            // this future completes, we get it back from the future and
            // put it back into `connection_receiver` again (since
            // the future we're constructing here always completes
            // with a value of `ProcessorKind::Receiver(connection)`.
            let next_connection = await_next_connection(
                connection_receiver.take().expect(
                    "somehow we fumbled the connection receiver between connections"
                ),
            )
            .boxed();

            // Add the "next connection" future to our collection.
            processors.push(next_connection);
        }

        // Wait until a connection or the "connection receiver" completes.
        let processors_in = processors;
        let (processor_kind, _, mut processors_out) =
            future::select_all(processors_in).await;

        // If it was the "connection receiver" future which completed, mark
        // that we will need to make a new one for the next loop.
        match processor_kind {
            ProcessorKind::Receiver {
                receiver: new_connection_receiver,
                connection_info,
            } => {
                connection_receiver.replace(new_connection_receiver);
                needs_next_connection = true;
                let handlers_ref = handlers.clone();
                processors_out.push(
                    async {
                        // Receive requests from the client and send back
                        // responses, until either the connection is closed or
                        // the connection is upgraded to another protocol.
                        let address = connection_info.address;
                        if let Err(error) =
                            handle_connection(connection_info, handlers_ref)
                                .await
                        {
                            // TODO: We probably want better error reporting up
                            // to the owner of this server, rather than
                            // printing to standard error stream like a pleb.
                            match error.source() {
                                Some(source) => eprintln!(
                                    "{}: error: {} ({})",
                                    address, error, source
                                ),
                                None => {
                                    eprintln!("{}: error: {}", address, error)
                                },
                            };
                        };
                        println!("{}: connection dropped", address);

                        // The output indicates that this is a future used to
                        // receive requests from a client and send back
                        // responses.
                        ProcessorKind::Connection
                    }
                    .boxed(),
                );
            },
            ProcessorKind::Connection => {
                needs_next_connection = false;
            },
        }

        // All incomplete futures go back to be collected next loop.
        // We may have received new ones too, so combine them.
        processors = processors_out;
    }
}

async fn handle_messages(
    work_in_receiver: mpsc::UnboundedReceiver<WorkerMessage>,
    listener_sender: mpsc::UnboundedSender<ListenerMessage>,
    handlers: Arc<Mutex<ResourceHandlerCollection>>,
) {
    // Drive to completion the stream of messages to the worker thread.
    work_in_receiver
        // The special `Exit` message completes the stream.
        .take_while(|message| future::ready(!matches!(message, WorkerMessage::Exit)))
        .for_each(|message| async {
            match message {
                // We already handled `Exit` in the `take_while` above;
                // it causes the stream to end early so we won't get this far.
                WorkerMessage::Exit => unreachable!(),

                WorkerMessage::StartListening {
                    listener,
                    result_sender,
                    connection_wrapper,
                } => {
                    // It's possible for the `send` here to fail, if the user
                    // of the library gave up waiting for the result of
                    // starting the server.  In that case we just drop
                    // the result, since obviously they didn't care about it.
                    result_sender
                        .send({
                            // This should never fail because `accept_connections`,
                            // which holds the receiver, never drops the receiver.
                            listener_sender
                                .unbounded_send(ListenerMessage::Start{
                                    listener,
                                    connection_wrapper
                                })
                                .expect("listener dropped before it reached the connection accepter");
                            Ok(())
                        })
                        .unwrap_or(());
                }

                WorkerMessage::RegisterHandler {
                    path,
                    handler,
                } => {
                    handlers.lock().expect(
                        "last thread that held handlers panicked"
                    ).insert(path, handler);
                },
            }
        })
        .await;
}

async fn worker(
    work_in_receiver: mpsc::UnboundedReceiver<WorkerMessage>,
    server_info: String,
) {
    // These channels are used to pass values between the various
    // sub-tasks below.
    // * listener sender/receiver passes TcpListener values from
    //   `handle_messages` to `accept_connections`.
    // * connection sender/receiver passes boxed Connection values from
    //   `accept_connections` to `handle_connections`.
    let handlers = Arc::new(Mutex::new(HashMap::new()));
    let (listener_sender, listener_receiver) = mpsc::unbounded();
    let (connection_sender, connection_receiver) = mpsc::unbounded();
    futures::select!(
        // Handle incoming messages to the worker thread, such as to stop,
        // store a new value, or try to retrieve back a value previously
        // stored.
        () = handle_messages(
            work_in_receiver,
            listener_sender,
            handlers.clone(),
        ).fuse() => {},

        () = accept_connections(listener_receiver, connection_sender, &server_info).fuse() => {},

        () = handle_connections(connection_receiver, handlers).fuse() => {},
    );
}

pub struct HttpServer {
    // This sender is used to deliver messages to the worker thread.
    work_in: mpsc::UnboundedSender<WorkerMessage>,

    // This is our handle to join the worker thread when dropped.
    worker: Option<std::thread::JoinHandle<()>>,
}

impl HttpServer {
    #[must_use]
    pub fn new<T>(server_info: T) -> Self
    where
        T: Into<String>,
    {
        // Make the channel used to communicate with the worker thread.
        let (sender, receiver) = mpsc::unbounded();

        // Store the sender end of the channel and spawn the worker thread,
        // giving it the receiver end as well as the TCP listener.
        let server_info = server_info.into();
        Self {
            work_in: sender,
            worker: Some(thread::spawn(|| {
                executor::block_on(worker(receiver, server_info))
            })),
        }
    }

    pub fn register<P>(
        &mut self,
        path: P,
        handler: Arc<ResourceHandler>,
    ) where
        P: Into<Vec<Vec<u8>>>,
    {
        self.work_in
            .unbounded_send(WorkerMessage::RegisterHandler {
                path: path.into(),
                handler,
            })
            .expect("worker message dropped before it could reach the worker");
    }

    pub async fn start_with_connection_wrapper<W>(
        &mut self,
        port: u16,
        connection_wrapper: W,
    ) -> Result<(), Error>
    where
        W: Fn(Box<dyn Connection>) -> ConnectionWrapFuture + Send + 'static,
    {
        let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, port))
            .await
            .map_err(Error::Bind)?;
        let (result_sender, result_receiver) = oneshot::channel();
        // It shouldn't be possible for this to fail, since the worker holds
        // the receiver for this channel, and isn't dropped until the server
        // itself is dropped.  So if it does fail, we want to know about it
        // since it would mean we have a bug.
        self.work_in
            .unbounded_send(WorkerMessage::StartListening {
                listener,
                result_sender,
                connection_wrapper: Box::new(connection_wrapper),
            })
            .expect("worker message dropped before it could reach the worker");
        // It shouldn't be possible for this to fail, since the worker will
        // always send us back an answer; it should never drop the sender of
        // the channel before doing so.  So if it does fail, we want to know
        // about it since it would mean we have a bug.
        result_receiver
            .await
            .expect("unable to receive result back from worker")
    }

    pub async fn start(
        &mut self,
        port: u16,
    ) -> Result<(), Error> {
        self.start_with_connection_wrapper(port, |connection| {
            async { Ok(connection) }.boxed()
        })
        .await
    }
}

impl Drop for HttpServer {
    fn drop(&mut self) {
        // Tell the worker thread to stop.
        //
        // It shouldn't be possible for this to fail, since the worker holds
        // the receiver for this channel, and we haven't joined or dropped the
        // worker yet (we will a few lines later).  So if it does fail, we want
        // to know about it since it would mean we have a bug.
        self.work_in
            .unbounded_send(WorkerMessage::Exit)
            .expect("worker message dropped before it could reach the worker");

        // Join the worker thread.
        //
        // This shouldn't fail unless the worker panics.  If it does, there's
        // no reason why we shouldn't panic as well.
        self.worker.take().expect(
            "somehow the worker thread join handle got lost before we could take it"
        ).join().expect(
            "the worker thread panicked before we could join it"
        );
    }
}
