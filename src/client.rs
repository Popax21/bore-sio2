//! Client implementation for the `bore` service.

use std::sync::Arc;

use anyhow::{bail, Context, Result};
use tokio::{io::AsyncWriteExt, net::TcpStream, time::timeout};
use tracing::{error, info, info_span, warn, Instrument};
use uuid::Uuid;

use crate::auth::Authenticator;
use crate::shared::{
    ClientMessage, Delimited, ForwardingEndpoint, ServerMessage, CONTROL_PORT, NETWORK_TIMEOUT,
};

/// State structure for the client.
pub struct Client {
    /// Control connection to the server.
    conn: Option<Delimited<TcpStream>>,

    /// Destination address of the server.
    to: String,

    // Local host that is forwarded.
    local_host: String,

    /// Local port that is forwarded.
    local_port: u16,

    /// Endpoint that is publicly available on the remote.
    remote_endpoint: ForwardingEndpoint,

    /// Optional secret used to authenticate clients.
    auth: Option<Authenticator>,
}

impl Client {
    /// Create a new client.
    pub async fn new(
        local_host: &str,
        local_port: u16,
        to: &str,
        endpoint: ForwardingEndpoint,
        secret: Option<&str>,
    ) -> Result<Self> {
        let mut stream = Delimited::new(connect_with_timeout(to, CONTROL_PORT).await?);
        let auth = secret.map(Authenticator::new);
        if let Some(auth) = &auth {
            auth.client_handshake(&mut stream).await?;
        }

        stream.send(ClientMessage::Hello(endpoint)).await?;
        let remote_endpoint = match stream.recv_timeout().await? {
            Some(ServerMessage::Hello(remote_endpoint)) => remote_endpoint,
            Some(ServerMessage::Error(message)) => bail!("server error: {message}"),
            Some(ServerMessage::Challenge(_)) => {
                bail!("server requires authentication, but no client secret was provided");
            }
            Some(_) => bail!("unexpected initial non-hello message"),
            None => bail!("unexpected EOF"),
        };
        info!(?remote_endpoint, "connected to server");

        match remote_endpoint {
            ForwardingEndpoint::Tcp(port) => info!("listening at {to}:{port}"),
            ForwardingEndpoint::Http => info!("listening at http[s]://{to}"),
        }

        Ok(Client {
            conn: Some(stream),
            to: to.to_string(),
            local_host: local_host.to_string(),
            local_port,
            remote_endpoint,
            auth,
        })
    }

    /// Returns the endpoint publicly available on the remote.
    pub fn remote_endpoint(&self) -> ForwardingEndpoint {
        self.remote_endpoint
    }

    /// Start the client, listening for new connections.
    pub async fn listen(mut self) -> Result<()> {
        let mut conn = self.conn.take().unwrap();
        let this = Arc::new(self);
        loop {
            match conn.recv().await? {
                Some(ServerMessage::Hello(_)) => warn!("unexpected hello"),
                Some(ServerMessage::Challenge(_)) => warn!("unexpected challenge"),
                Some(ServerMessage::Heartbeat) => (),
                Some(ServerMessage::Connection(id)) => {
                    let this = Arc::clone(&this);
                    tokio::spawn(
                        async move {
                            info!("new connection");
                            match this.handle_connection(id).await {
                                Ok(_) => info!("connection exited"),
                                Err(err) => warn!(%err, "connection exited with error"),
                            }
                        }
                        .instrument(info_span!("proxy", %id)),
                    );
                }
                Some(ServerMessage::Error(err)) => error!(%err, "server error"),
                None => return Ok(()),
            }
        }
    }

    async fn handle_connection(&self, id: Uuid) -> Result<()> {
        let mut remote_conn =
            Delimited::new(connect_with_timeout(&self.to[..], CONTROL_PORT).await?);
        if let Some(auth) = &self.auth {
            auth.client_handshake(&mut remote_conn).await?;
        }
        remote_conn.send(ClientMessage::Accept(id)).await?;
        let mut local_conn = connect_with_timeout(&self.local_host, self.local_port).await?;
        let mut parts = remote_conn.into_parts();
        debug_assert!(parts.write_buf.is_empty(), "framed write buffer not empty");
        local_conn.write_all(&parts.read_buf).await?; // mostly of the cases, this will be empty
        tokio::io::copy_bidirectional(&mut local_conn, &mut parts.io).await?;
        Ok(())
    }
}

async fn connect_with_timeout(to: &str, port: u16) -> Result<TcpStream> {
    match timeout(NETWORK_TIMEOUT, TcpStream::connect((to, port))).await {
        Ok(res) => res,
        Err(err) => Err(err.into()),
    }
    .with_context(|| format!("could not connect to {to}:{port}"))
}
