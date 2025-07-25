//! Server implementation for the `bore` service.

use std::fmt::Debug;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::pin::Pin;
use std::{io, ops::RangeInclusive, sync::Arc, time::Duration};

use anyhow::{Error, Result};
use dashmap::DashMap;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UnixListener, UnixStream};
use tokio::time::{sleep, timeout};
use tracing::{info, info_span, warn, Instrument};
use uuid::Uuid;

use crate::auth::Authenticator;
use crate::shared::{ClientMessage, Delimited, ForwardingEndpoint, ServerMessage, CONTROL_PORT};

/// State structure for the server.
pub struct Server {
    /// Range of TCP ports that can be forwarded.
    port_range: RangeInclusive<u16>,

    /// Optional secret used to authenticate clients.
    auth: Option<Authenticator>,

    /// Concurrent map of IDs to incoming connections.
    conns: Arc<DashMap<Uuid, Pin<Box<dyn Connection>>>>,

    /// IP address where the control server will bind to.
    bind_addr: IpAddr,

    /// IP address where tunnels will listen on.
    bind_tunnels: IpAddr,

    /// UNIX socket which is created for HTTP connections.
    http_socket: Option<PathBuf>,
}

impl Server {
    /// Create a new server with a specified minimum port number.
    pub fn new(port_range: RangeInclusive<u16>, secret: Option<&str>) -> Self {
        assert!(!port_range.is_empty(), "must provide at least one port");
        Server {
            port_range,
            conns: Arc::new(DashMap::new()),
            auth: secret.map(Authenticator::new),
            bind_addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            bind_tunnels: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            http_socket: None,
        }
    }

    /// Set the IP address where tunnels will listen on.
    pub fn set_bind_addr(&mut self, bind_addr: IpAddr) {
        self.bind_addr = bind_addr;
    }

    /// Set the IP address where the control server will bind to.
    pub fn set_bind_tunnels(&mut self, bind_tunnels: IpAddr) {
        self.bind_tunnels = bind_tunnels;
    }

    /// Sets the UNIX socket which is opened for HTTP connections.
    pub fn set_http_socket(&mut self, http_socket: Option<PathBuf>) {
        self.http_socket = http_socket;
    }

    /// Start the server, listening for new connections.
    pub async fn listen(self) -> Result<()> {
        let this = Arc::new(self);
        let listener = TcpListener::bind((this.bind_addr, CONTROL_PORT)).await?;
        info!(addr = ?this.bind_addr, "server listening");

        loop {
            let (stream, addr) = listener.accept().await?;
            let this = Arc::clone(&this);
            tokio::spawn(
                async move {
                    info!("incoming connection");
                    if let Err(err) = this.handle_connection(stream).await {
                        warn!(%err, "connection exited with error");
                    } else {
                        info!("connection exited");
                    }
                }
                .instrument(info_span!("control", ?addr)),
            );
        }
    }

    async fn create_tcp_listener(&self, port: u16) -> Result<TcpListener, &'static str> {
        let try_bind = |port: u16| async move {
            TcpListener::bind((self.bind_tunnels, port))
                .await
                .map_err(|err| match err.kind() {
                    io::ErrorKind::AddrInUse => "port already in use",
                    io::ErrorKind::PermissionDenied => "permission denied",
                    _ => "failed to bind to port",
                })
        };
        if port > 0 {
            // Client requests a specific port number.
            if !self.port_range.contains(&port) {
                return Err("client port number not in allowed range");
            }
            try_bind(port).await
        } else {
            // Client requests any available port in range.
            //
            // In this case, we bind to 150 random port numbers. We choose this value because in
            // order to find a free port with probability at least 1-δ, when ε proportion of the
            // ports are currently available, it suffices to check approximately -2 ln(δ) / ε
            // independently and uniformly chosen ports (up to a second-order term in ε).
            //
            // Checking 150 times gives us 99.999% success at utilizing 85% of ports under these
            // conditions, when ε=0.15 and δ=0.00001.
            for _ in 0..150 {
                let port = fastrand::u16(self.port_range.clone());
                match try_bind(port).await {
                    Ok(listener) => return Ok(listener),
                    Err(_) => continue,
                }
            }
            Err("failed to find an available port")
        }
    }

    async fn create_http_listener(&self) -> Result<UnixListener, &'static str> {
        let Some(socket) = &self.http_socket else {
            return Err("server doesn't support HTTP forwarding");
        };

        match UnixListener::bind(socket) {
            Ok(l) => Ok(l),
            Err(err)
                if [io::ErrorKind::AddrInUse, io::ErrorKind::AlreadyExists]
                    .contains(&err.kind()) =>
            {
                Err("an HTTP forwarding session is already in progress")
            }
            Err(_) => Err("failed to bind to HTTP socket"),
        }
    }

    async fn handle_connection(&self, stream: TcpStream) -> Result<()> {
        let mut stream = Delimited::new(stream);
        if let Some(auth) = &self.auth {
            if let Err(err) = auth.server_handshake(&mut stream).await {
                warn!(%err, "server handshake failed");
                stream.send(ServerMessage::Error(err.to_string())).await?;
                return Ok(());
            }
        }

        match stream.recv_timeout().await? {
            Some(ClientMessage::Authenticate(_)) => {
                warn!("unexpected authenticate");
                Ok(())
            }
            Some(ClientMessage::Hello(ForwardingEndpoint::Tcp(port))) => {
                let listener = match self.create_tcp_listener(port).await {
                    Ok(listener) => listener,
                    Err(err) => {
                        stream.send(ServerMessage::Error(err.into())).await?;
                        return Ok(());
                    }
                };
                let host = listener.local_addr()?.ip();
                let port = listener.local_addr()?.port();
                info!(?host, ?port, "opened TCP listener");
                self.handle_forwarding_client(stream, ForwardingEndpoint::Tcp(port), listener).await
            }
            Some(ClientMessage::Hello(ForwardingEndpoint::Http)) => {
                let listener = match self.create_http_listener().await {
                    Ok(listener) => listener,
                    Err(err) => {
                        stream.send(ServerMessage::Error(err.into())).await?;
                        return Ok(());
                    }
                };
                info!("opened HTTP listener");
                self.handle_forwarding_client(stream, ForwardingEndpoint::Http, listener).await?;
                _ = std::fs::remove_file(self.http_socket.as_ref().unwrap());
                Ok(())
            }
            Some(ClientMessage::Accept(id)) => {
                info!(%id, "forwarding connection");
                match self.conns.remove(&id) {
                    Some((_, mut stream2)) => {
                        let mut parts = stream.into_parts();
                        debug_assert!(parts.write_buf.is_empty(), "framed write buffer not empty");
                        stream2.write_all(&parts.read_buf).await?;
                        tokio::io::copy_bidirectional(&mut parts.io, &mut stream2).await?;
                    }
                    None => warn!(%id, "missing connection"),
                }
                Ok(())
            }
            None => Ok(()),
        }
    }

    async fn handle_forwarding_client(
        &self,
        mut stream: Delimited<TcpStream>,
        endpoint: ForwardingEndpoint,
        listener: impl Listener,
    ) -> Result<()> {
        stream.send(ServerMessage::Hello(endpoint)).await?;

        loop {
            if stream.send(ServerMessage::Heartbeat).await.is_err() {
                // Assume that the TCP connection has been dropped.
                return Ok(());
            }
            const TIMEOUT: Duration = Duration::from_millis(500);
            if let Ok(result) = timeout(TIMEOUT, listener.accept()).await {
                let (stream2, addr) = result?;
                info!(?addr, ?endpoint, "new connection");

                let id = Uuid::new_v4();
                let conns = Arc::clone(&self.conns);

                conns.insert(id, Box::pin(stream2));
                tokio::spawn(async move {
                    // Remove stale entries to avoid memory leaks.
                    sleep(Duration::from_secs(10)).await;
                    if conns.remove(&id).is_some() {
                        warn!(%id, "removed stale connection");
                    }
                });
                stream.send(ServerMessage::Connection(id)).await?;
            }
        }
    }
}

trait Listener {
    type Connection: Connection;
    type Address: Debug;
    async fn accept(&self) -> Result<(Self::Connection, Self::Address)>;
}
trait Connection: AsyncRead + AsyncWrite + Send + Sync + 'static {}

impl Listener for TcpListener {
    type Connection = TcpStream;
    type Address = SocketAddr;

    async fn accept(&self) -> Result<(Self::Connection, Self::Address)> {
        self.accept().await.map_err(Error::from)
    }
}
impl Connection for TcpStream {}

impl Listener for UnixListener {
    type Connection = UnixStream;
    type Address = tokio::net::unix::SocketAddr;

    async fn accept(&self) -> Result<(Self::Connection, Self::Address)> {
        self.accept().await.map_err(Error::from)
    }
}
impl Connection for UnixStream {}
