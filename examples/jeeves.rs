use anyhow::{
    anyhow,
    Context as _,
};
use async_ctrlc::CtrlC;
use async_tls::TlsAcceptor;
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
    ConnectionWrapper,
    ConnectionWrapperResult,
    FetchResults,
    HttpServer,
    ResourceFuture,
};
use rustls::{
    internal::pemfile::{
        certs,
        pkcs8_private_keys,
    },
    NoClientAuth,
};
use std::{
    fs::File,
    io::BufReader,
    path::PathBuf,
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
    /// Port number on which to serve requests
    #[structopt(default_value = "8080")]
    port: u16,

    /// Use Transport Layer Security (TLS)
    #[structopt(long)]
    tls: bool,

    /// SSL certificate (if TLS is enabled)
    #[structopt(long, default_value = "cert.pem")]
    cert: PathBuf,

    /// SSL private key (if TLS is enabled)
    #[structopt(long, default_value = "key.pem")]
    key: PathBuf,
}

fn load_tls_config(
    cert_file_name: PathBuf,
    key_file_name: PathBuf,
) -> anyhow::Result<rustls::ServerConfig> {
    let certs = certs(&mut BufReader::new(
        File::open(&cert_file_name)
            .context("unable to load SSL certificate")?,
    ))
    .map_err(|()| anyhow!("unable to extract SSL certificate"))?;
    let key = pkcs8_private_keys(&mut BufReader::new(
        File::open(&key_file_name).context("unable to load SSL private key")?,
    ))
    .map_err(|()| anyhow!("unable to extract SSL private key"))
    .and_then(|mut keys| {
        if keys.is_empty() {
            Err(anyhow!("no SSK private key found in file"))
        } else {
            Ok(keys.remove(0))
        }
    })?;
    let mut config = rustls::ServerConfig::new(NoClientAuth::new());
    config
        .set_single_cert(certs, key)
        .context("unable to configure TLS with SSL certificate/key")?;
    Ok(config)
}

async fn tls_wrap_connection(
    connection: Box<dyn Connection>,
    tls_config: Arc<rustls::ServerConfig>,
) -> ConnectionWrapperResult {
    Ok(Box::new(TlsAcceptor::from(tls_config).accept(connection).await?))
}

fn make_connection_tls_wrapper(
    cert: PathBuf,
    key: PathBuf,
) -> anyhow::Result<Box<ConnectionWrapper>> {
    let tls_config = Arc::new(load_tls_config(cert, key)?);
    Ok(Box::new(move |connection| {
        Box::pin(tls_wrap_connection(connection, tls_config.clone()))
    }))
}

fn make_connection_identity_wrapper() -> Box<ConnectionWrapper> {
    Box::new(|connection| async { Ok(connection) }.boxed())
}

fn make_connection_wrapper(
    tls: bool,
    cert: PathBuf,
    key: PathBuf,
) -> anyhow::Result<Box<ConnectionWrapper>> {
    if tls {
        make_connection_tls_wrapper(cert, key)
    } else {
        Ok(make_connection_identity_wrapper())
    }
}

async fn run_server() -> anyhow::Result<()> {
    let Opts {
        port,
        tls,
        cert,
        key,
    } = Opts::from_args();
    let mut server = HttpServer::new("jeeves");
    server.register(
        &[b"".to_vec(), b"foo".to_vec()][..],
        Arc::new(handle_request_factory),
    );
    let connection_wrapper = make_connection_wrapper(tls, cert, key)?;
    server
        .start_with_connection_wrapper(port, connection_wrapper)
        .await
        .context("unable to start HTTP server")?;
    futures::future::pending().await
}

async fn main_async() {
    if let Err(error) = run_server().await {
        eprintln!("{:?}", error);
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
