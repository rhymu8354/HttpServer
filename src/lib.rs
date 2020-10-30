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
    AsyncRead,
    AsyncReadExt,
    AsyncWrite,
    AsyncWriteExt,
    executor,
    future,
    FutureExt,
    StreamExt,
};
use rhymuweb::{
    Request,
    Response,
};
use std::{
    cell::RefCell,
    thread,
};

pub trait Connection: AsyncRead + AsyncWrite + Send + Unpin + 'static {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin + 'static> Connection for T {}

pub struct FetchResults {
    pub response: Response,
    pub connection: Box<dyn Connection>,
}

type ResourceHandler = dyn FnMut(
    Request,
    Box<dyn Connection>,
    Vec<u8>,
) -> FetchResults; // + Send + Unpin + 'static;

#[derive(Debug)]
enum WorkerMessage {
    // This tells the worker thread to terminate.
    Exit,

    // This tells the worker thread to start listening for incoming requests
    // from HTTP clients.
    StartListening{
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
                if let Ok((connection, address)) = current_listener.accept().await {
                    println!("accept_connections: Connection received from {}", address);
                    listener.replace(Some(current_listener));
                    connection_sender.unbounded_send(Box::new(connection));
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
                        new_listener.local_addr().unwrap().port()
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
    Receiver,
}

async fn handle_connections(
    mut connection_receiver: mpsc::UnboundedReceiver<Box<dyn Connection>>,
) {
    let mut processors = Vec::new();
    let mut needs_next_connection = true;
    loop {
        // Add a special future to receive the next connection.
        if needs_next_connection {
            let next_connection = async {
                // If `connection_receiver` completes (has no next), it means
                // the sender end, which is held by `accept_connections`, was
                // dropped somehow.  This should never happen, since that
                // future never completes.
                let connection = connection_receiver.next().await.unwrap();

                processors.push(async {
                    // TODO: Here is where we will be receiving the request
                    // from the HTTP client, sending it to a request handler,
                    // and sending back a response (possibly in a loop).

                    // The output indicates that this is the future used
                    // to receive the next connection.
                    ProcessorKind::Connection
                }.boxed());

                // The output indicates that this is the future used
                // to receive the next connection.
                ProcessorKind::Receiver
            }.boxed();

            // Add the "next connection" future to our collection.
            processors.push(next_connection);
        }

        // Wait until a connection or the "connection receiver" completes.
        let (processor_kind, _, processors_left) = future::select_all(
            processors.into_iter()
        ).await;

        // If it was the "connection receiver" future which completed, mark
        // that we will need to make a new one for the next loop.
        needs_next_connection = matches!(processor_kind, ProcessorKind::Receiver);

        // All incomplete futures go back to be collected next loop.
        processors = processors_left;
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
        .take_while(|message| future::ready(
            !matches!(message, WorkerMessage::Exit)
        ))
        .for_each(|message| async {
            println!("handle_messages: got message {:?}", message);
            match message {
                // We already handled `Exit` in the `take_while` above;
                // it causes the stream to end early so we won't get this far.
                WorkerMessage::Exit => unreachable!(),

                WorkerMessage::StartListening{
                    listener,
                    result_sender
                } => {
                    result_sender.send({
                        println!(
                            "handle_messages: Now listening for connections on port {}.",
                            listener.local_addr().unwrap().port()
                        );
                        // This should never fail because `accept_connections`,
                        // which holds the receiver, never drops the receiver.
                        listener_sender.unbounded_send(
                            ListenerMessage::Start(listener)
                        ).unwrap();
                        Ok(())
                    }).unwrap_or(());
                },
            }
        }).await;
    println!("handle_messages: exiting");
}

async fn worker(
    work_in_receiver: mpsc::UnboundedReceiver<WorkerMessage>
) {
    // These channels are used to pass values between the various
    // sub-tasks below.
    // * listener sender/receiver passes TcpListener values from
    //   `handle_messages` to `accept_connections`.
    // * connection sender/receiver passes boxed Connection values
    //   from `accept_connections` to `handle_connections`.
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
            worker: Some(
                thread::spawn(|| executor::block_on(
                    worker(receiver)
                )),
            ),
        }
    }

    pub fn register<P>(
        &mut self,
        path: P,
        resource_handler: Box<ResourceHandler>
    )
        where P: Into<Vec<Vec<u8>>>
    {
    }

    pub async fn start(
        &mut self,
        port: u16,
        _use_tls: bool,
    ) -> Result<(), Error> {
        let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)).await
            .map_err(Error::Bind)?;
        let (result_sender, result_receiver) = oneshot::channel();
        // It shouldn't be possible for this to fail, since the worker holds
        // the receiver for this channel, and isn't dropped until the client
        // itself is dropped.  So if it does fail, we want to know about it
        // since it would mean we have a bug.
        self.work_in.unbounded_send(WorkerMessage::StartListening{
            listener,
            result_sender,
        }).unwrap();
        // It shouldn't be possible for this to fail, since the worker will
        // always send us back an answer; it should never drop the sender of
        // the channel before doing so.  So if it does fail, we want to know
        // about it since it would mean we have a bug.
        result_receiver.await.unwrap()
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
        self.work_in.unbounded_send(WorkerMessage::Exit).unwrap();

        // Join the worker thread.
        //
        // This shouldn't fail unless the worker panics.  If it does, there's
        // no reason why we shouldn't panic as well.
        self.worker.take().unwrap().join().unwrap();
    }
}
