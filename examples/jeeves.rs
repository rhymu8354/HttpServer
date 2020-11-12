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
use structopt::StructOpt;

fn handle_request_factory(
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
            on_upgraded: None,
        }
    }
    .boxed()
}

#[derive(Clone, StructOpt)]
struct Opts {
    /// URI of resource to request
    #[structopt(default_value = "8080")]
    port: u16,
}

async fn main_async() {
    let opts: Opts = Opts::from_args();
    let mut server = HttpServer::new("jeeves");
    server.register(
        &[b"".to_vec(), b"foo".to_vec()][..],
        Arc::new(handle_request_factory),
    );
    match server.start(opts.port, std::convert::identity).await {
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
