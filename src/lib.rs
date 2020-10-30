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
    pub connection: Box<dyn Connection>,
}

type ResourceHandler =
    dyn FnMut(Request, Box<dyn Connection>, Vec<u8>) -> FetchResults; // + Send + Unpin + 'static;

#[derive(Debug)]
enum WorkerMessage {
    // This tells the worker thread to terminate.
    Exit,

    // This tells the worker thread to start listening for incoming requests
    // from HTTP clients.
    StartListening {
        listener: TcpListener,
        result_sender: oneshot::Sender<Result<(), Error>>,
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
    mut connection: Box<dyn Connection>
) -> Result<(Request, Box<dyn Connection>, Vec<u8>), Error> {
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
            return Ok((request, connection, receive_buffer));
        }
    }
}

async fn handle_connection(
    connection: Box<dyn Connection>
) -> Result<Box<dyn Connection>, Error> {
    // Assemble HTTP request from incoming data.
    let (_, mut connection, _) = receive_request(connection).await?;

    // Come up with a response and send it.
    // TODO: Dispatch request to handler (use default "not
    // found" handler if we can't find one) to produce an HTTP
    // response.
    //
    // For now, just make a 404 "Not Found" response.
    let mut response = Response::new();
    response.status_code = 404;
    response.reason_phrase = "Not Found".into();

    // Send the HTTP response back through the connection.
    let raw_response = response.generate().map_err(Error::BadResponse)?;
    connection.write_all(&raw_response).await.map_err(Error::UnableToSend)?;
    Ok(connection)
}

async fn await_next_connection(
    mut connection_receiver: mpsc::UnboundedReceiver<Box<dyn Connection>>,
    processors: Arc<Mutex<Vec<Processor>>>,
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
    processors.lock().expect("last thread that held processors panicked").push(
        async {
            // Assemble HTTP request from incoming data.
            match handle_connection(connection).await {
                Ok(_) => {
                    // TODO: We might want to hold onto the connection
                    // in case the client is going to send another request.
                },
                Err(error) => {
                    match error.source() {
                        Some(source) => {
                            eprintln!("error: {} ({})", error, source)
                        },
                        None => eprintln!("error: {}", error),
                    };
                },
            }

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
    connection_receiver: mpsc::UnboundedReceiver<Box<dyn Connection>>
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
) {
    // Drive to completion the stream of messages to the worker thread.
    println!("handle_messages: processing messages");
    work_in_receiver
        // The special `Stop` message completes the stream.
        .take_while(|message| future::ready(!matches!(message, WorkerMessage::Exit)))
        .for_each(|message| async {
            println!("handle_messages: got message {:?}", message);
            match message {
                // We already handled `Exit` in the `take_while` above;
                // it causes the stream to end early so we won't get this far.
                WorkerMessage::Exit => unreachable!(),

                WorkerMessage::StartListening {
                    listener,
                    result_sender,
                } => {
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
    let (listener_sender, listener_receiver) = mpsc::unbounded();
    let (connection_sender, connection_receiver) = mpsc::unbounded();
    futures::select!(
        // Handle incoming messages to the worker thread, such as to stop,
        // store a new value, or try to retrieve back a value previously
        // stored.
        () = handle_messages(
            work_in_receiver,
            listener_sender
        ).fuse() => (),

        () = accept_connections(listener_receiver, connection_sender).fuse() => (),

        () = handle_connections(connection_receiver).fuse() => (),
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
        resource_handler: Box<ResourceHandler>,
    ) where
        P: Into<Vec<Vec<u8>>>,
    {
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
