use std::{net::IpAddr, path::PathBuf};

use anyhow::Result;
use bore_cli::{client::Client, server::Server, shared::ForwardingEndpoint};
use clap::{error::ErrorKind, CommandFactory, Parser, Subcommand};
use serde::Deserialize;

#[derive(Parser, Debug)]
#[clap(author, version, about)]
struct Args {
    #[clap(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Starts a local proxy to the remote server.
    #[clap(disable_help_flag(true))]
    Local {
        /// The local port to expose.
        #[clap(env = "BORE_LOCAL_PORT")]
        local_port: u16,

        /// The local host to expose.
        #[clap(short, long, value_name = "HOST", default_value = "localhost")]
        local_host: String,

        /// Address of the remote server to expose local ports to.
        #[clap(short, long, env = "BORE_SERVER")]
        to: Option<String>,

        /// Optional port on the remote server to select.
        #[clap(short, long, default_value_t = 0)]
        port: u16,

        /// Expose the port via HTTP[S] on the remote server.
        #[clap(short, long)]
        http: bool,

        /// Optional secret for authentication.
        #[clap(short, long, env = "BORE_SECRET", hide_env_values = true)]
        secret: Option<String>,
    },

    /// Runs the remote proxy server.
    Server {
        /// Minimum accepted TCP port number.
        #[clap(long, default_value_t = 1024, env = "BORE_MIN_PORT")]
        min_port: u16,

        /// Maximum accepted TCP port number.
        #[clap(long, default_value_t = 65535, env = "BORE_MAX_PORT")]
        max_port: u16,

        /// Optional secret for authentication.
        #[clap(short, long, env = "BORE_SECRET", hide_env_values = true)]
        secret: Option<String>,

        /// IP address to bind to, clients must reach this.
        #[clap(long, default_value = "0.0.0.0")]
        bind_addr: IpAddr,

        /// IP address where tunnels will listen on, defaults to --bind-addr.
        #[clap(long)]
        bind_tunnels: Option<IpAddr>,

        /// UNIX socket to create for HTTP connections.
        #[clap(long)]
        http_socket: Option<PathBuf>,
    },
}

#[derive(Default, Deserialize)]
struct Config {
    server: Option<String>,
    secret: Option<String>,
}

#[tokio::main]
async fn run(command: Command) -> Result<()> {
    match command {
        Command::Local {
            local_host,
            local_port,
            to,
            port,
            http,
            secret,
        } => {
            let config_path = std::env::var("XDG_CONFIG_HOME").map_or_else(
                |_| std::env::home_dir().unwrap().join(".config"),
                PathBuf::from,
            ).join("bore.toml");

            let config: Config = if config_path.exists() {
                toml::from_str(
                    &std::fs::read_to_string(config_path).expect("failed to read config file"),
                )
                .expect("failed to parse config file")
            } else {
                Default::default()
            };

            let (to, secret) = to.map_or((config.server, config.secret), |t| (Some(t), secret));
            let Some(to) = to else {
                Args::command()
                    .error(
                        ErrorKind::MissingRequiredArgument,
                        "no server was specified, and none was found in the config",
                    )
                    .exit();
            };

            let endpoint = if http {
                if port != 0 {
                    Args::command()
                        .error(
                            ErrorKind::ArgumentConflict,
                            "both --port and --http were specified at the same time",
                        )
                        .exit();
                }

                ForwardingEndpoint::Http
            } else {
                ForwardingEndpoint::Tcp(port)
            };

            let client =
                Client::new(&local_host, local_port, &to, endpoint, secret.as_deref()).await?;
            client.listen().await?;
        }
        Command::Server {
            min_port,
            max_port,
            secret,
            bind_addr,
            bind_tunnels,
            http_socket,
        } => {
            let port_range = min_port..=max_port;
            if port_range.is_empty() {
                Args::command()
                    .error(ErrorKind::InvalidValue, "port range is empty")
                    .exit();
            }
            let mut server = Server::new(port_range, secret.as_deref());
            server.set_bind_addr(bind_addr);
            server.set_bind_tunnels(bind_tunnels.unwrap_or(bind_addr));
            server.set_http_socket(http_socket);
            server.listen().await?;
        }
    }

    Ok(())
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    run(Args::parse().command)
}
