use async_compression::tokio::bufread::GzipEncoder;
use hudsucker::{
    async_trait::async_trait,
    certificate_authority::CertificateAuthority,
    decode_request, decode_response,
    hyper::{
        header::CONTENT_ENCODING,
        server::conn::AddrStream,
        service::{make_service_fn, service_fn},
        Body, Method, Request, Response, Server, StatusCode,
    },
    HttpContext, HttpHandler, ProxyBuilder, RequestOrResponse,
};
use reqwest::tls::Certificate;
use std::{
    convert::Infallible,
    net::{SocketAddr, TcpListener},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};
use tokio::sync::oneshot::Sender;
use tokio_util::io::ReaderStream;

pub const HELLO_WORLD: &str = "Hello, World!";

async fn test_server(req: Request<Body>) -> Result<Response<Body>, Infallible> {
    match (req.method(), req.uri().path()) {
        (&Method::GET, "/hello") => Ok(Response::new(Body::from(HELLO_WORLD))),
        (&Method::GET, "/hello/gzip") => Ok(Response::builder()
            .header(CONTENT_ENCODING, "gzip")
            .status(StatusCode::OK)
            .body(Body::wrap_stream(ReaderStream::new(GzipEncoder::new(
                HELLO_WORLD.as_bytes(),
            ))))
            .unwrap()),
        (&Method::POST, "/echo") => Ok(Response::new(req.into_body())),
        _ => Ok(Response::new(Body::empty())),
    }
}

pub fn start_test_server() -> Result<(SocketAddr, Sender<()>), Box<dyn std::error::Error>> {
    let make_svc = make_service_fn(|_conn: &AddrStream| async {
        Ok::<_, Infallible>(service_fn(test_server))
    });

    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))?;
    let addr = listener.local_addr()?;

    let (tx, rx) = tokio::sync::oneshot::channel();

    tokio::spawn(
        Server::from_tcp(listener)?
            .serve(make_svc)
            .with_graceful_shutdown(async { rx.await.unwrap_or_default() }),
    );

    Ok((addr, tx))
}

pub fn start_proxy(
    ca: impl CertificateAuthority,
) -> Result<(SocketAddr, TestHandler, Sender<()>), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))?;
    let addr = listener.local_addr()?;
    let (tx, rx) = tokio::sync::oneshot::channel();

    let http_handler = TestHandler::new();

    let proxy = ProxyBuilder::new()
        .with_listener(listener)
        .with_rustls_client()
        .with_ca(ca)
        .with_http_handler(http_handler.clone())
        .build();

    tokio::spawn(proxy.start(async {
        rx.await.unwrap_or_default();
    }));

    Ok((addr, http_handler, tx))
}

pub fn start_noop_proxy(
    ca: impl CertificateAuthority,
) -> Result<(SocketAddr, Sender<()>), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))?;
    let addr = listener.local_addr()?;
    let (tx, rx) = tokio::sync::oneshot::channel();

    let proxy = ProxyBuilder::new()
        .with_listener(listener)
        .with_rustls_client()
        .with_ca(ca)
        .build();

    tokio::spawn(proxy.start(async {
        rx.await.unwrap_or_default();
    }));

    Ok((addr, tx))
}

pub fn build_client(proxy: &str) -> reqwest::Client {
    let proxy = reqwest::Proxy::all(proxy).unwrap();

    let ca_cert = Certificate::from_pem(include_bytes!("../../examples/ca/hudsucker.cer")).unwrap();

    reqwest::Client::builder()
        .proxy(proxy)
        .add_root_certificate(ca_cert)
        .build()
        .unwrap()
}

#[derive(Clone)]
pub struct TestHandler {
    pub request_counter: Arc<AtomicUsize>,
    pub response_counter: Arc<AtomicUsize>,
}

impl TestHandler {
    pub fn new() -> Self {
        Self {
            request_counter: Arc::new(AtomicUsize::new(0)),
            response_counter: Arc::new(AtomicUsize::new(0)),
        }
    }
}

#[async_trait]
impl HttpHandler for TestHandler {
    async fn handle_request(
        &mut self,
        _ctx: &HttpContext,
        req: Request<Body>,
    ) -> RequestOrResponse {
        self.request_counter.fetch_add(1, Ordering::Relaxed);
        let req = decode_request(req).unwrap();
        RequestOrResponse::Request(req)
    }

    async fn handle_response(&mut self, _ctx: &HttpContext, res: Response<Body>) -> Response<Body> {
        self.response_counter.fetch_add(1, Ordering::Relaxed);
        decode_response(res).unwrap()
    }
}