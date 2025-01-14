use crate::config::ClientConfig;
use crate::config::Ipv6DualStackConfig;
use crate::config::ServerConfig;
use crate::connection::Connection;
use crate::driver::streams::session::StreamSession;
use crate::driver::streams::ProtoReadError;
use crate::driver::streams::ProtoWriteError;
use crate::driver::utils::varint_w2q;
use crate::driver::Driver;
use crate::error::ConnectingError;
use crate::error::ConnectionError;
use quinn::TokioRuntime;
use socket2::Domain as SocketDomain;
use socket2::Protocol as SocketProtocol;
use socket2::Socket;
use socket2::Type as SocketType;
use std::collections::HashMap;
use std::future::Future;
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::net::SocketAddrV4;
use std::net::SocketAddrV6;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use tokio::net::lookup_host;
use tracing::debug;
use url::Host;
use url::Url;
use wtransport_proto::error::ErrorCode;
use wtransport_proto::frame::FrameKind;
use wtransport_proto::headers::Headers;
use wtransport_proto::session::SessionRequest as SessionRequestProto;
use wtransport_proto::session::SessionResponse as SessionResponseProto;

/// Type of endpoint accepting multiple WebTransport connections.
pub struct Server;

/// Type of endpoint opening a WebTransport connection.
pub struct Client;

/// Entrypoint for creating client or server connections.
///
/// * For creating a server: [`Endpoint::server`].
/// * For creating a client: [`Endpoint::client`].
pub struct Endpoint<Side> {
    endpoint: quinn::Endpoint,
    _marker: PhantomData<Side>,
}

impl<Side> Endpoint<Side> {
    fn bind_socket(
        bind_address: SocketAddr,
        dual_stack_config: Ipv6DualStackConfig,
    ) -> std::io::Result<Socket> {
        let domain = match bind_address {
            SocketAddr::V4(_) => SocketDomain::IPV4,
            SocketAddr::V6(_) => SocketDomain::IPV6,
        };

        let socket = Socket::new(domain, SocketType::DGRAM, Some(SocketProtocol::UDP))?;

        match dual_stack_config {
            Ipv6DualStackConfig::OsDefault => {}
            Ipv6DualStackConfig::Deny => socket.set_only_v6(true)?,
            Ipv6DualStackConfig::Allow => socket.set_only_v6(false)?,
        }

        socket.bind(&bind_address.into())?;

        Ok(socket)
    }

    /// Waits for all connections on the endpoint to be cleanly shut down.
    pub async fn wait_idle(&self) {
        self.endpoint.wait_idle().await;
    }
}

impl Endpoint<Server> {
    /// Constructs a *server* endpoint.
    pub fn server(server_config: ServerConfig) -> std::io::Result<Self> {
        let quic_config = server_config.quic_config;
        let socket =
            Self::bind_socket(server_config.bind_address, server_config.dual_stack_config)?;
        let runtime = Arc::new(TokioRuntime);

        let endpoint = quinn::Endpoint::new(
            quinn::EndpointConfig::default(),
            Some(quic_config),
            socket.into(),
            runtime,
        )?;

        Ok(Self {
            endpoint,
            _marker: PhantomData,
        })
    }

    /// Get the next incoming connection attempt from a client.
    pub async fn accept(&self) -> IncomingSession {
        let quic_connecting = self
            .endpoint
            .accept()
            .await
            .expect("Endpoint cannot be closed");

        debug!("New incoming QUIC connection");

        IncomingSession::new(quic_connecting)
    }
}

impl Endpoint<Client> {
    /// Constructs a *client* endpoint.
    pub fn client(client_config: ClientConfig) -> std::io::Result<Self> {
        let quic_config = client_config.quic_config;
        let socket =
            Self::bind_socket(client_config.bind_address, client_config.dual_stack_config)?;
        let runtime = Arc::new(TokioRuntime);

        let mut endpoint = quinn::Endpoint::new(
            quinn::EndpointConfig::default(),
            None,
            socket.into(),
            runtime,
        )?;

        endpoint.set_default_client_config(quic_config);

        Ok(Self {
            endpoint,
            _marker: PhantomData,
        })
    }

    /// Connects to a remote endpoint.
    ///
    /// `server_name` must be covered by the certificate presented by the server.
    pub async fn connect<S>(&self, url: S) -> Result<Connection, ConnectingError>
    where
        S: AsRef<str>,
    {
        let url = Url::parse(url.as_ref())
            .map_err(|parse_error| ConnectingError::InvalidUrl(parse_error.to_string()))?;

        if url.scheme() != "https" {
            return Err(ConnectingError::InvalidUrl(
                "WebTransport URL scheme must be 'https'".to_string(),
            ));
        }

        let host = url.host().expect("https scheme must have an host");
        let port = url.port().unwrap_or(443);

        let (socket_address, server_name) = match host {
            Host::Domain(domain) => {
                let socket_address = lookup_host(format!("{domain}:{port}"))
                    .await
                    .map_err(ConnectingError::DnsLookup)?
                    .next()
                    .ok_or(ConnectingError::DnsNotFound)?;
                (socket_address, domain.to_string())
            }
            Host::Ipv4(address) => {
                let socket_address = SocketAddr::V4(SocketAddrV4::new(address, port));
                (socket_address, address.to_string())
            }
            Host::Ipv6(address) => {
                let socket_address = SocketAddr::V6(SocketAddrV6::new(address, port, 0, 0));
                (socket_address, address.to_string())
            }
        };

        let quic_connection = self
            .endpoint
            .connect(socket_address, &server_name)
            .expect("QUIC connection parameters must be validated")
            .await
            .map_err(|connection_error| {
                ConnectingError::ConnectionError(connection_error.into())
            })?;

        let driver = Driver::init(quic_connection.clone());

        let _settings = driver.accept_settings().await.map_err(|driver_error| {
            ConnectingError::ConnectionError(ConnectionError::with_driver_error(
                driver_error,
                &quic_connection,
            ))
        })?;

        // TODO(biagio): validate settings

        let session_request_proto =
            SessionRequestProto::new(url.as_ref()).expect("Url has been already validate");

        let mut stream_session = match driver.open_session(session_request_proto).await {
            Ok(stream_session) => stream_session,
            Err(driver_error) => {
                return Err(ConnectingError::ConnectionError(
                    ConnectionError::with_driver_error(driver_error, &quic_connection),
                ))
            }
        };

        let stream_id = stream_session.id();
        let session_id = stream_session.session_id();

        match stream_session
            .write_frame(stream_session.request().headers().generate_frame(stream_id))
            .await
        {
            Ok(()) => {}
            Err(ProtoWriteError::Stopped) => {
                return Err(ConnectingError::SessionRejected);
            }
            Err(ProtoWriteError::NotConnected) => {
                return Err(ConnectingError::with_no_connection(&quic_connection));
            }
        }

        let frame = match stream_session.read_frame().await {
            Ok(frame) => frame,
            Err(ProtoReadError::H3(error_code)) => {
                quic_connection.close(varint_w2q(error_code.to_code()), b"");
                return Err(ConnectingError::ConnectionError(
                    ConnectionError::local_h3_error(error_code),
                ));
            }
            Err(ProtoReadError::IO(_io_error)) => {
                return Err(ConnectingError::with_no_connection(&quic_connection));
            }
        };

        if !matches!(frame.kind(), FrameKind::Headers) {
            quic_connection.close(varint_w2q(ErrorCode::FrameUnexpected.to_code()), b"");
            return Err(ConnectingError::ConnectionError(
                ConnectionError::local_h3_error(ErrorCode::FrameUnexpected),
            ));
        }

        let headers = match Headers::with_frame(&frame, stream_id) {
            Ok(headers) => headers,
            Err(error_code) => {
                quic_connection.close(varint_w2q(error_code.to_code()), b"");
                return Err(ConnectingError::ConnectionError(
                    ConnectionError::local_h3_error(error_code),
                ));
            }
        };

        let session_response = match SessionResponseProto::try_from(headers) {
            Ok(session_response) => session_response,
            Err(_) => {
                quic_connection.close(varint_w2q(ErrorCode::Message.to_code()), b"");
                return Err(ConnectingError::ConnectionError(
                    ConnectionError::local_h3_error(ErrorCode::Message),
                ));
            }
        };

        if session_response.code().is_successful() {
            match driver.register_session(stream_session).await {
                Ok(()) => {}
                Err(driver_error) => {
                    return Err(ConnectingError::ConnectionError(
                        ConnectionError::with_driver_error(driver_error, &quic_connection),
                    ))
                }
            }
        } else {
            return Err(ConnectingError::SessionRejected);
        }

        Ok(Connection::new(quic_connection, driver, session_id))
    }
}

type DynFutureIncomingSession =
    dyn Future<Output = Result<SessionRequest, ConnectionError>> + Send + Sync;

/// [`Future`] for an in-progress incoming connection attempt.
///
/// Created by [`Endpoint::accept`].
pub struct IncomingSession(Pin<Box<DynFutureIncomingSession>>);

impl IncomingSession {
    fn new(quic_connecting: quinn::Connecting) -> Self {
        Self(Box::pin(Self::accept(quic_connecting)))
    }

    async fn accept(quic_connecting: quinn::Connecting) -> Result<SessionRequest, ConnectionError> {
        let quic_connection = quic_connecting.await?;

        let driver = Driver::init(quic_connection.clone());

        let _settings = driver.accept_settings().await.map_err(|driver_error| {
            ConnectionError::with_driver_error(driver_error, &quic_connection)
        })?;

        // TODO(biagio): validate settings

        let stream_session = driver.accept_session().await.map_err(|driver_error| {
            ConnectionError::with_driver_error(driver_error, &quic_connection)
        })?;

        Ok(SessionRequest::new(quic_connection, driver, stream_session))
    }
}

impl Future for IncomingSession {
    type Output = Result<SessionRequest, ConnectionError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Future::poll(self.0.as_mut(), cx)
    }
}

/// A incoming client session request.
///
/// Server should use methods [`accept`](Self::accept) or [`not_found`](Self::not_found)
/// in order to validate or reject the client request.
pub struct SessionRequest {
    quic_connection: quinn::Connection,
    driver: Driver,
    stream_session: StreamSession,
}

impl SessionRequest {
    pub(crate) fn new(
        quic_connection: quinn::Connection,
        driver: Driver,
        stream_session: StreamSession,
    ) -> Self {
        Self {
            quic_connection,
            driver,
            stream_session,
        }
    }

    /// Returns the `:authority` field of the request.
    pub fn authority(&self) -> &str {
        self.stream_session.request().authority()
    }

    /// Returns the `:path` field of the request.
    pub fn path(&self) -> &str {
        self.stream_session.request().path()
    }

    /// Returns the `origin` field of the request if present.
    pub fn origin(&self) -> Option<&str> {
        self.stream_session.request().origin()
    }

    /// Returns the `user-agent` field of the request if present.
    pub fn user_agent(&self) -> Option<&str> {
        self.stream_session.request().user_agent()
    }

    /// Returns all header fields associated with the request.
    pub fn headers(&self) -> &HashMap<String, String> {
        self.stream_session.request().headers().as_ref()
    }

    /// Accepts the client request and it establishes the WebTransport session.
    pub async fn accept(mut self) -> Result<Connection, ConnectionError> {
        let user_agent = self.user_agent().unwrap_or_default();

        let mut response = SessionResponseProto::ok();

        // Chrome support
        if !user_agent.contains("firefox") {
            response.add("sec-webtransport-http3-draft", "draft02");
        }

        self.send_response(response).await?;

        let session_id = self.stream_session.session_id();

        self.driver
            .register_session(self.stream_session)
            .await
            .map_err(|driver_error| {
                ConnectionError::with_driver_error(driver_error, &self.quic_connection)
            })?;

        Ok(Connection::new(
            self.quic_connection,
            self.driver,
            session_id,
        ))
    }

    /// Rejects the client request by replying with `404` status code.
    pub async fn not_found(mut self) {
        let user_agent = self.user_agent().unwrap_or_default();

        let mut response = SessionResponseProto::not_found();

        // Chrome support
        if !user_agent.contains("firefox") {
            response.add("sec-webtransport-http3-draft", "draft02");
        }

        let _ = self.send_response(response).await;
        self.stream_session.finish().await;
    }

    async fn send_response(
        &mut self,
        response: SessionResponseProto,
    ) -> Result<(), ConnectionError> {
        let frame = response.headers().generate_frame(self.stream_session.id());

        match self.stream_session.write_frame(frame).await {
            Ok(()) => Ok(()),
            Err(ProtoWriteError::NotConnected) => {
                Err(ConnectionError::no_connect(&self.quic_connection))
            }
            Err(ProtoWriteError::Stopped) => {
                self.quic_connection
                    .close(varint_w2q(ErrorCode::ClosedCriticalStream.to_code()), b"");

                Err(ConnectionError::local_h3_error(
                    ErrorCode::ClosedCriticalStream,
                ))
            }
        }
    }
}
