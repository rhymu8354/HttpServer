use async_ctrlc::CtrlC;
use futures::{
    future::FutureExt,
    select,
};
use rhymuweb::{
    Request,
    Response,
};
use rhymuweb_server::{
    Connection,
    FetchResults,
    HttpServer,
    ResourceFuture,
};
use std::{
    error::Error as _,
    sync::Arc,
};

fn handle_request(
    _request: Request,
    connection: Box<dyn Connection>,
) -> ResourceFuture {
    async {
        let mut response = Response::new();
        response.status_code = 200;
        response.reason_phrase = "OK".into();
        response.body = b"Hello, World!".to_vec();
        response
            .headers
            .set_header("Content-Type", "text/plain; charset=utf-8");
        FetchResults {
            response,
            connection,
        }
    }
    .boxed()
}

async fn main_async() {
    let mut server = HttpServer::new();
    server.register(
        &[b"".to_vec(), b"foo".to_vec()][..],
        Arc::new(handle_request),
    );
    match server.start(8080, false).await {
        Ok(()) => futures::future::pending().await,
        Err(error) => {
            match error.source() {
                Some(source) => eprintln!("error: {} ({})", error, source),
                None => eprintln!("error: {}", error),
            };
        },
    }
}

fn main() {
    futures::executor::block_on(async {
        select!(
            () = main_async().fuse() => (),
            () = CtrlC::new().unwrap().fuse() => {
                println!("(Ctrl+C pressed; aborted)");
            },
        )
    });
}
