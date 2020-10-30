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
) {
    println!("accept_connections: entered");
    let listener: RefCell<Option<TcpListener>> = RefCell::new(None);
    loop {
        let accepter = async {
            let current_listener = listener.borrow_mut().take();
            if let Some(current_listener) = current_listener {
                println!("accept_connections: Accepting next connection");
                if let Ok((connection, address)) = current_listener.accept().await {
                    println!("accept_connections: Connection received from {}", address);
                    listener.replace(Some(current_listener));
                } else {
                    // TODO: This happens if the listener breaks somehow.
                    // We should set up a mechanism for reporting this.
                }
            } else {
                futures::future::pending().await
            }
        };
        let message_handler = async {
            // If there's no listener, there should be a kicker, so this
            // shouldn't fail.  If it does, we have a bug and want a panic to
            // see it while debugging.
            println!("accept_connections: Waiting for messages");
            let message = listener_receiver.next().await;
            match message.unwrap() {
                ListenerMessage::Start(new_listener) => {
                    println!(
                        "accept_connections: Now accepting connections on port {}",
                        new_listener.local_addr().unwrap().port()
                    );
                    listener.replace(Some(new_listener));
                },
            }
        };
        futures::select!(
            () = accepter.fuse() => (),
            () = message_handler.fuse() => (),
        );
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
    // We will be in charge of the listener for incoming connections.
    let (listener_sender, listener_receiver) = mpsc::unbounded();
    futures::select!(
        // Handle incoming messages to the worker thread, such as to stop,
        // store a new value, or try to retrieve back a value previously
        // stored.
        () = handle_messages(
            work_in_receiver,
            listener_sender
        ).fuse() => (),

        () = accept_connections(listener_receiver).fuse() => (),

        // () = handle_requests()
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
