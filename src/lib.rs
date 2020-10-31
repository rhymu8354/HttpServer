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
};
pub use error::Error;
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
    pin::Pin,
    sync::{
        Arc,
        Mutex,
    },
    thread,
};

pub trait Connection: AsyncRead + AsyncWrite + Send + Unpin + 'static {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin + 'static> Connection for T {}

pub struct FetchResults {
    pub response: Response,
    pub connection: Option<Box<dyn Connection>>,
}

pub type ResourceFuture =
    Pin<Box<dyn Future<Output = FetchResults> + Send + 'static>>;

type ResourceHandler = dyn Fn(Request, Box<dyn Connection>, Vec<u8>) -> ResourceFuture
    + Send
    + Sync
    + Unpin
    + 'static;

type ResourceHandlerCollection = HashMap<Vec<Vec<u8>>, Arc<ResourceHandler>>;

enum WorkerMessage {
    // This tells the worker thread to terminate.
    Exit,

    // This tells the worker thread to start listening for incoming requests
    // from HTTP clients.
    StartListening {
        listener: TcpListener,
        result_sender: oneshot::Sender<Result<(), Error>>,
    },

    RegisterHandler {
        path: Vec<Vec<u8>>,
        handler: Arc<ResourceHandler>,
    },
}

enum ListenerMessage {
    Start(TcpListener),
    // Stop,
}

async fn accept_connections(
    mut listener_receiver: mpsc::UnboundedReceiver<ListenerMessage>,
    connection_sender: mpsc::UnboundedSender<Box<dyn Connection>>,
) {
    // Listener is in a `RefCell` because two separate futures need to access
    // it:
    // * `accepter` takes the listener, uses it, and potentially puts it back.
    // * `message_handler` places new listeners there (potentially dropping any
    //   previous listeners that might still be there).
    println!("accept_connections: entered");
    let listener: RefCell<Option<TcpListener>> = RefCell::new(None);
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
                println!("accept_connections: Accepting next connection");
                if let Ok((connection, address)) =
                    current_listener.accept().await
                {
                    println!(
                        "accept_connections: Connection received from {}",
                        address
                    );
                    if listener.borrow().is_none() {
                        listener.replace(Some(current_listener));
                    }
                    // This should never fail because the connection receiver
                    // never completes.  Each connection will be at least
                    // accepted by the processor future constructed to
                    // handle it.
                    connection_sender
                        .unbounded_send(Box::new(connection))
                        .expect(
                            "connection dropped before it reached a processor",
                        );
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
            println!("accept_connections: Waiting for messages");
            match listener_receiver.next().await {
                Some(ListenerMessage::Start(new_listener)) => {
                    println!(
                        "accept_connections: Now accepting connections on port {}",
                        new_listener.local_addr().expect(
                            "we should have been able to figure out our own address"
                        ).port()
                    );
                    listener.replace(Some(new_listener));
                },
                None => futures::future::pending().await,
            }
        };
        futures::select!(
            () = accepter.fuse() => (),
            () = message_handler.fuse() => (),
        );
    }
}

// This is the type of value returned by a completed future selected by
// `accept_connections`.  It's used to tell the difference between a future
// which processed a connection from a future which processed the connection
// receiver.
enum ProcessorKind {
    Connection,
    Receiver(mpsc::UnboundedReceiver<Box<dyn Connection>>),
}

type Processor = Pin<Box<dyn Future<Output = ProcessorKind> + Send>>;

async fn receive_request(
    connection: &mut dyn Connection
) -> Result<(Request, Vec<u8>), Error> {
    let mut request = Request::new();
    let mut receive_buffer = Vec::new();
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
    connection: Box<dyn Connection>,
    handlers: Arc<Mutex<ResourceHandlerCollection>>,
) -> Result<(), Error> {
    let mut connection_origin = Some(connection);
    loop {
        // Assemble HTTP request from incoming data.
        let mut connection = connection_origin
            .take()
            .expect("we somehow dropped the connection");
        let (request, trailer) = receive_request(&mut connection).await?;

        // Peek into the request to see if we should close this connection
        // after the response has been sent.
        let close_after_response =
            request.headers.has_header_token("Connection", "close");

        // Dispatch request to handler (use default "not
        // found" handler if we can't find one) to produce an HTTP
        // response.
        let handler_factory_reference = handlers
            .lock()
            .expect("")
            .get(request.target.path())
            .map(Clone::clone);
        let (mut response, connection) =
            if let Some(handler_factory) = handler_factory_reference {
                let handler = handler_factory(request, connection, trailer);
                let mut fetch_results = handler.await;
                if !fetch_results.response.body.is_empty() {
                    fetch_results.response.headers.set_header(
                        "Content-Length",
                        fetch_results.response.body.len().to_string(),
                    );
                }
                (fetch_results.response, fetch_results.connection)
            } else {
                let mut response = Response::new();
                response.status_code = 404;
                response.reason_phrase = "Not Found".into();
                (response, Some(connection))
            };

        if let Some(mut connection) = connection {
            // If we're supposed to close the connection after sending
            // the response, tell the client so via the "close" token
            // in the "Connection" header.
            if close_after_response {
                let mut tokens = response.headers.header_tokens("Connection");
                if tokens.iter().all(|token| token != "close") {
                    tokens.push("close".into());
                    response
                        .headers
                        .set_header("Connection", tokens.join(", "));
                }
            }

            // Send the HTTP response back through the connection.
            let raw_response =
                response.generate().map_err(Error::BadResponse)?;
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
            } else {
                connection_origin.replace(connection);
            }
        } else {
            return Ok(());
        }
    }
}

async fn await_next_connection(
    mut connection_receiver: mpsc::UnboundedReceiver<Box<dyn Connection>>,
    processors: Arc<Mutex<Vec<Processor>>>,
    handlers: Arc<Mutex<ResourceHandlerCollection>>,
) -> ProcessorKind {
    // Wait for the next connection to come in.
    //
    // If `connection_receiver` completes (has no next), it means
    // the sender end, which is held by `accept_connections`, was
    // dropped somehow.  This should never happen, since that
    // future never completes.
    let next_connection = connection_receiver.next();
    let connection = next_connection.await.expect(
        "this task receives connections from other task which should never complete"
    );
    let handlers_ref = handlers.clone();
    processors.lock().expect("last thread that held processors panicked").push(
        async {
            // Assemble HTTP request from incoming data.
            if let Err(error) =
                handle_connection(connection, handlers_ref).await
            {
                // TODO: We probably want better error reporting up to
                // the owner of this server, rather than printing to
                // standard error stream like a pleb.
                match error.source() {
                    Some(source) => eprintln!("error: {} ({})", error, source),
                    None => eprintln!("error: {}", error),
                };
            };

            // The output indicates that this is the future used
            // to receive the next connection.
            ProcessorKind::Connection
        }
        .boxed(),
    );

    // The output indicates that this is the future used
    // to receive the next connection.
    ProcessorKind::Receiver(connection_receiver)
}

async fn handle_connections(
    connection_receiver: mpsc::UnboundedReceiver<Box<dyn Connection>>,
    handlers: Arc<Mutex<ResourceHandlerCollection>>,
) {
    let processors = Arc::new(Mutex::new(Vec::new()));
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
                processors.clone(),
                handlers.clone(),
            )
            .boxed();

            // Add the "next connection" future to our collection.
            processors
                .lock()
                .expect("last thread that held processors panicked")
                .push(next_connection);
        }

        // Wait until a connection or the "connection receiver" completes.
        let processors_in = std::mem::take(
            &mut *processors
                .lock()
                .expect("last thread that held processors panicked"),
        );
        let (processor_kind, _, processors_out) =
            future::select_all(processors_in).await;

        // If it was the "connection receiver" future which completed, mark
        // that we will need to make a new one for the next loop.
        match processor_kind {
            ProcessorKind::Receiver(new_connection_receiver) => {
                connection_receiver.replace(new_connection_receiver);
                needs_next_connection = true;
            },
            ProcessorKind::Connection => {
                needs_next_connection = false;
            },
        }

        // All incomplete futures go back to be collected next loop.
        // We may have received new ones too, so combine them.
        processors
            .lock()
            .expect("last thread that held processors panicked")
            .extend(processors_out);
    }
}

async fn handle_messages(
    work_in_receiver: mpsc::UnboundedReceiver<WorkerMessage>,
    listener_sender: mpsc::UnboundedSender<ListenerMessage>,
    handlers: Arc<Mutex<ResourceHandlerCollection>>,
) {
    // Drive to completion the stream of messages to the worker thread.
    println!("handle_messages: processing messages");
    work_in_receiver
        // The special `Stop` message completes the stream.
        .take_while(|message| future::ready(!matches!(message, WorkerMessage::Exit)))
        .for_each(|message| async {
            match message {
                // We already handled `Exit` in the `take_while` above;
                // it causes the stream to end early so we won't get this far.
                WorkerMessage::Exit => unreachable!(),

                WorkerMessage::StartListening {
                    listener,
                    result_sender,
                } => {
                    println!("handle_messages: got StartListening message");
                    // It's possible for the `send` here to fail, if the user
                    // of the library gave up waiting for the result of
                    // starting the server.  In that case we just drop
                    // the result, since obviously they didn't care about it.
                    result_sender
                        .send({
                            println!(
                                "handle_messages: Now listening for connections on port {}.",
                                listener.local_addr().expect(
                                    "we should have been able to figure out our own address"
                                ).port()
                            );
                            // This should never fail because `accept_connections`,
                            // which holds the receiver, never drops the receiver.
                            listener_sender
                                .unbounded_send(ListenerMessage::Start(listener))
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
    println!("handle_messages: exiting");
}

async fn worker(work_in_receiver: mpsc::UnboundedReceiver<WorkerMessage>) {
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
        ).fuse() => (),

        () = accept_connections(listener_receiver, connection_sender).fuse() => (),

        () = handle_connections(connection_receiver, handlers).fuse() => (),
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
    pub fn new() -> Self {
        // Make the channel used to communicate with the worker thread.
        let (sender, receiver) = mpsc::unbounded();

        // Store the sender end of the channel and spawn the worker thread,
        // giving it the receiver end as well as the TCP listener.
        Self {
            work_in: sender,
            worker: Some(thread::spawn(|| {
                executor::block_on(worker(receiver))
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

    pub async fn start(
        &mut self,
        port: u16,
        _use_tls: bool,
    ) -> Result<(), Error> {
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
}

impl Default for HttpServer {
    fn default() -> Self {
        Self::new()
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
