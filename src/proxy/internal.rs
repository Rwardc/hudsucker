use crate::{
    certificate_authority::CertificateAuthority, HttpContext, HttpHandler, RequestOrResponse,
    Rewind, WebSocketContext, WebSocketHandler,
};
use futures::{Sink, Stream, StreamExt};
use http::uri::{Authority, Scheme};
use hyper::{
    client::connect::Connect, header::Entry, server::conn::Http, service::service_fn,
    upgrade::Upgraded, Body, Client, Method, Request, Response, StatusCode, Uri,
};
use std::{convert::Infallible, future::Future, net::SocketAddr, sync::Arc};
use hyper_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite},
    net::TcpStream,
    task::JoinHandle,
};
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::{tungstenite::{self, Message}, Connector, WebSocketStream, MaybeTlsStream};
use tracing::{error, info_span, instrument, warn, Instrument, Span};
use tracing::log::debug;

fn bad_request() -> Response<Body> {
    Response::builder()
        .status(StatusCode::BAD_REQUEST)
        .body(Body::empty())
        .expect("Failed to build response")
}

fn spawn_with_trace<T: Send + Sync + 'static>(
    fut: impl Future<Output = T> + Send + 'static,
    span: Span,
) -> JoinHandle<T> {
    tokio::spawn(fut.instrument(span))
}

pub(crate) struct InternalProxy<C, CA, H, W> {
    pub ca: Arc<CA>,
    pub client: Client<C>,
    pub http_handler: H,
    pub websocket_handler: W,
    pub websocket_connector: Option<Connector>,
    pub client_addr: SocketAddr,
}

impl<C, CA, H, W> Clone for InternalProxy<C, CA, H, W>
where
    C: Clone,
    H: Clone,
    W: Clone,
{
    fn clone(&self) -> Self {
        InternalProxy {
            ca: Arc::clone(&self.ca),
            client: self.client.clone(),
            http_handler: self.http_handler.clone(),
            websocket_handler: self.websocket_handler.clone(),
            websocket_connector: self.websocket_connector.clone(),
            client_addr: self.client_addr,
        }
    }
}

impl<C, CA, H, W> InternalProxy<C, CA, H, W>
where
    C: Connect + Clone + Send + Sync + 'static,
    CA: CertificateAuthority,
    H: HttpHandler,
    W: WebSocketHandler,
{
    fn context(&self) -> HttpContext {
        HttpContext {
            client_addr: self.client_addr,
        }
    }

    #[instrument(
        skip_all,
        fields(
            version = ?req.version(),
            method = %req.method(),
            uri=%req.uri(),
            client_addr = %self.client_addr,
        )
    )]
    pub(crate) async fn proxy(mut self, req: Request<Body>) -> Result<Response<Body>, Infallible> {
        let ctx = self.context();

        let req = match self
            .http_handler
            .handle_request(&ctx, req)
            .instrument(info_span!("handle_request"))
            .await
        {
            RequestOrResponse::Request(req) => req,
            RequestOrResponse::Response(res) => return Ok(res),
        };

        if req.method() == Method::CONNECT {
            Ok(self.process_connect(req))
        } else if hyper_tungstenite::is_upgrade_request(&req) {
            let upgrade_result = self.upgrade_websocket(req).await;
            Ok(upgrade_result)
        } else {
            let res = self
                .client
                .request(normalize_request(req))
                .instrument(info_span!("proxy_request"))
                .await;

            match res {
                Ok(res) => Ok(self
                    .http_handler
                    .handle_response(&ctx, res)
                    .instrument(info_span!("handle_response"))
                    .await),
                Err(err) => Ok(self
                    .http_handler
                    .handle_error(&ctx, err)
                    .instrument(info_span!("handle_error"))
                    .await),
            }
        }
    }

    fn process_connect(mut self, mut req: Request<Body>) -> Response<Body> {
        match req.uri().authority().cloned() {
            Some(authority) => {
                let span = info_span!("process_connect");
                let fut = async move {
                    match hyper::upgrade::on(&mut req).await {
                        Ok(mut upgraded) => {
                            let mut buffer = [0; 4];
                            let bytes_read = match upgraded.read(&mut buffer).await {
                                Ok(bytes_read) => bytes_read,
                                Err(e) => {
                                    error!("Failed to read from upgraded connection: {}", e);
                                    return;
                                }
                            };

                            let mut upgraded = Rewind::new_buffered(
                                upgraded,
                                bytes::Bytes::copy_from_slice(buffer[..bytes_read].as_ref()),
                            );

                            if self
                                .http_handler
                                .should_intercept(&self.context(), &req)
                                .await
                            {
                                if buffer == *b"GET " {
                                    if let Err(e) =
                                        self.serve_stream(upgraded, Scheme::HTTP, authority).await
                                    {
                                        error!("WebSocket connect error: {}", e);
                                    }

                                    return;
                                } else if buffer[..2] == *b"\x16\x03" {
                                    let server_config = self
                                        .ca
                                        .gen_server_config(&authority)
                                        .instrument(info_span!("gen_server_config"))
                                        .await;

                                    let stream = match TlsAcceptor::from(server_config)
                                        .accept(upgraded)
                                        .await
                                    {
                                        Ok(stream) => stream,
                                        Err(e) => {
                                            error!("Failed to establish TLS connection: {}", e);
                                            return;
                                        }
                                    };

                                    if let Err(e) =
                                        self.serve_stream(stream, Scheme::HTTPS, authority).await
                                    {
                                        if !e
                                            .to_string()
                                            .starts_with("error shutting down connection")
                                        {
                                            error!("HTTPS connect error: {}", e);
                                        }
                                    }

                                    return;
                                } else {
                                    warn!(
                                        "Unknown protocol, read '{:02X?}' from upgraded connection",
                                        &buffer[..bytes_read]
                                    );
                                }
                            }

                            let mut server = match TcpStream::connect(authority.as_ref()).await {
                                Ok(server) => server,
                                Err(e) => {
                                    error!("Failed to connect to {}: {}", authority, e);
                                    return;
                                }
                            };

                            if let Err(e) =
                                tokio::io::copy_bidirectional(&mut upgraded, &mut server).await
                            {
                                error!("Failed to tunnel to {}: {}", authority, e);
                            }
                        }
                        Err(e) => error!("Upgrade error: {}", e),
                    };
                };

                spawn_with_trace(fut, span);
                Response::new(Body::empty())
            }
            None => bad_request(),
        }
    }

    #[instrument(skip_all)]
    async fn upgrade_websocket(self, req: Request<Body>) -> Response<Body> {
        let mut req = {
            let (mut parts, _) = req.into_parts();

            parts.uri = {
                let mut parts = parts.uri.into_parts();

                parts.scheme = if parts.scheme.unwrap_or(Scheme::HTTP) == Scheme::HTTP {
                    Some("ws".try_into().expect("Failed to convert scheme"))
                } else {
                    Some("wss".try_into().expect("Failed to convert scheme"))
                };

                match Uri::from_parts(parts) {
                    Ok(uri) => uri,
                    Err(_) => {
                        return bad_request();
                    }
                }
            };

            Request::from_parts(parts, ())
        };
        let mut config = WebSocketConfig::default();
        config.read_as_frames = true;
        // 2. Upgrade the connection using the negotiated response received by the proxy client
        match hyper_tungstenite::upgrade(&mut req, Some(config)) {
            Ok((_, websocket)) => {
                // Ignore the fabricated response returned by hyper_tungstenite. We don't
                // know what the server is going to accept in the negotiation, so use the
                // response collected from the client connection above.
                #[cfg(not(any(feature = "rustls-client", feature = "native-tls-client")))]
                let client_fut = tokio_tungstenite::connect_async(req);
                let uri = req.uri().clone();
                // 1. Connect to the server using the client's original request, awaiting the
                //    negotiated response.
                #[cfg(any(feature = "rustls-client", feature = "native-tls-client"))]
                let client_fut = tokio_tungstenite::connect_async_tls_with_config(
                    req,
                    Some(config.clone()),
                    false,
                    self.websocket_connector.clone(),
                );
                let Ok((mut client_socket, resp)) =
                    client_fut.await
                else {
                    return bad_request()
                };
                let span = info_span!("websocket");
                let fut = async move {
                    match websocket.await {
                        Ok(ws) => {
                            if let Err(e) =
                                self.handle_websocket(ws, client_socket, uri).await {
                                error!("Failed to handle WebSocket: {}", e);
                            }
                        }
                        Err(e) => {
                            error!("Failed to upgrade to WebSocket: {}", e);
                            if let Err(e) = client_socket
                                .close(None)
                                .await {
                                    error!("Could not close client socket after failed websocket upgrade: {}", e)
                            }
                        }
                    }
                };

                spawn_with_trace(fut, span);
                let parts = resp.into_parts();
                Response::from_parts(parts.0, parts.1.map_or(Body::empty(), |b| Body::from(b)))
            }
            Err(_) => bad_request(),
        }
    }

    #[instrument(skip_all)]
    async fn handle_websocket(
        self,
        server_socket: WebSocketStream<Upgraded>,
        client_socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
        uri: Uri
    ) -> Result<(), tungstenite::Error> {

        let (server_sink, server_stream) = server_socket.split();
        let (client_sink, client_stream) = client_socket.split();

        let InternalProxy {
            websocket_handler, ..
        } = self;

        spawn_message_forwarder(
            server_stream,
            client_sink,
            websocket_handler.clone(),
            WebSocketContext::ServerToClient {
                src: uri.clone(),
                dst: self.client_addr,
            },
        );

        spawn_message_forwarder(
            client_stream,
            server_sink,
            websocket_handler,
            WebSocketContext::ClientToServer {
                src: self.client_addr,
                dst: uri,
            },
        );

        Ok(())
    }

    #[instrument(skip_all)]
    async fn serve_stream<I>(
        self,
        stream: I,
        scheme: Scheme,
        authority: Authority,
    ) -> Result<(), hyper::Error>
    where
        I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let service = service_fn(|mut req| {
            if req.version() == hyper::Version::HTTP_10 || req.version() == hyper::Version::HTTP_11
            {
                let (mut parts, body) = req.into_parts();

                parts.uri = {
                    let mut parts = parts.uri.into_parts();
                    parts.scheme = Some(scheme.clone());
                    parts.authority = Some(authority.clone());
                    Uri::from_parts(parts).expect("Failed to build URI")
                };

                req = Request::from_parts(parts, body);
            };

            self.clone().proxy(req)
        });

        Http::new()
            .serve_connection(stream, service)
            .with_upgrades()
            .await
    }
}

fn spawn_message_forwarder(
    stream: impl Stream<Item = Result<Message, tungstenite::Error>> + Unpin + Send + 'static,
    sink: impl Sink<Message, Error = tungstenite::Error> + Unpin + Send + 'static,
    handler: impl WebSocketHandler,
    ctx: WebSocketContext,
) {
    let span = info_span!("message_forwarder", context = ?ctx);
    let fut = handler.handle_websocket(ctx, stream, sink);
    spawn_with_trace(fut, span);
}

#[instrument(skip_all)]
fn normalize_request<T>(mut req: Request<T>) -> Request<T> {
    // Hyper will automatically add a Host header if needed.
    req.headers_mut().remove(hyper::header::HOST);

    // HTTP/2 supports multiple cookie headers, but HTTP/1.x only supports one.
    if let Entry::Occupied(mut cookies) = req.headers_mut().entry(hyper::header::COOKIE) {
        let joined_cookies = bstr::join(b"; ", cookies.iter());
        cookies.insert(joined_cookies.try_into().expect("Failed to join cookies"));
    }

    *req.version_mut() = hyper::Version::HTTP_11;
    req
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_rustls::rustls::ServerConfig;

    struct CA;

    #[async_trait::async_trait]
    impl CertificateAuthority for CA {
        async fn gen_server_config(&self, _authority: &Authority) -> Arc<ServerConfig> {
            unimplemented!();
        }
    }

    fn build_proxy(
    ) -> InternalProxy<hyper::client::HttpConnector, CA, crate::NoopHandler, crate::NoopHandler>
    {
        InternalProxy {
            ca: Arc::new(CA),
            client: hyper::Client::new(),
            http_handler: crate::NoopHandler::new(),
            websocket_handler: crate::NoopHandler::new(),
            websocket_connector: None,
            client_addr: "127.0.0.1:8080".parse().unwrap(),
        }
    }

    mod bad_request {
        use super::*;

        #[test]
        fn correct_status() {
            let res = bad_request();
            assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        }
    }

    mod normalize_request {
        use super::*;

        #[test]
        fn removes_host_header() {
            let req = Request::builder()
                .uri("http://example.com/")
                .header(hyper::header::HOST, "example.com")
                .body(())
                .unwrap();

            let req = normalize_request(req);

            assert_eq!(req.headers().get(hyper::header::HOST), None);
        }

        #[test]
        fn joins_cookies() {
            let req = Request::builder()
                .uri("http://example.com/")
                .header(hyper::header::COOKIE, "foo=bar")
                .header(hyper::header::COOKIE, "baz=qux")
                .body(())
                .unwrap();

            let req = normalize_request(req);

            assert_eq!(
                req.headers().get_all(hyper::header::COOKIE).iter().count(),
                1
            );

            assert_eq!(
                req.headers().get(hyper::header::COOKIE),
                Some(&"foo=bar; baz=qux".parse().unwrap())
            );
        }
    }

    mod process_connect {
        use super::*;

        #[test]
        fn returns_bad_request_if_missing_authority() {
            let proxy = build_proxy();

            let req = Request::builder()
                .uri("/foo/bar?baz")
                .body(Body::empty())
                .unwrap();

            let res = proxy.process_connect(req);

            assert_eq!(res.status(), StatusCode::BAD_REQUEST)
        }
    }

    mod upgrade_websocket {
        use super::*;

        #[test]
        fn returns_bad_request_if_missing_authority() {
            let proxy = build_proxy();

            let req = Request::builder()
                .uri("/foo/bar?baz")
                .body(Body::empty())
                .unwrap();

            let res = proxy.upgrade_websocket(req);

            assert_eq!(res.status(), StatusCode::BAD_REQUEST)
        }

        #[test]
        fn returns_bad_request_if_missing_headers() {
            let proxy = build_proxy();

            let req = Request::builder()
                .uri("http://example.com/foo/bar?baz")
                .body(Body::empty())
                .unwrap();

            let res = proxy.upgrade_websocket(req);

            assert_eq!(res.status(), StatusCode::BAD_REQUEST)
        }
    }
}
