use clap::{Parser, Subcommand};
use pingora::ErrorType::InvalidHTTPHeader;
use pingora::prelude::HttpPeer;
use reqwest::StatusCode;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Parser)]
#[command(version, about)]
struct Args {
    #[clap(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Start the reverse proxy.
    Serve {
        /// Path to the config file.
        config: String,

        /// The address to bind to.
        #[arg(short, long, default_value = "127.0.0.1:3030")]
        address: String,
    },
}

#[derive(Debug, Deserialize, Clone)]
pub struct AppConfig {
    pub address: std::net::SocketAddr,
    pub start: String,
    #[serde(default)]
    pub wait_period: Option<u64>,
    #[serde(default)]
    pub health_check: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(flatten)]
    pub apps: HashMap<String, AppConfig>,
}

pub struct YarpProxy {
    config: Arc<Config>,
}

impl YarpProxy {
    fn new(config: Config) -> Self {
        Self {
            config: Arc::new(config),
        }
    }
}

#[async_trait::async_trait]
impl pingora::prelude::ProxyHttp for YarpProxy {
    type CTX = Option<AppConfig>;
    fn new_ctx(&self) -> Option<AppConfig> {
        None
    }

    async fn early_request_filter(
        &self,
        session: &mut pingora::prelude::Session,
        ctx: &mut Self::CTX,
    ) -> pingora::Result<()> {
        let host = session
            .downstream_session
            .get_header("host")
            .ok_or_else(|| pingora::Error::explain(InvalidHTTPHeader, "No host header detected"))?
            .to_str()
            .map_err(|_| {
                pingora::Error::explain(
                    InvalidHTTPHeader,
                    "Failed to convert host header to string",
                )
            })?;

        *ctx = self.config.apps.get(host).map(|c| c.to_owned());

        Ok(())
    }

    async fn upstream_peer(
        &self,
        session: &mut pingora::proxy::Session,
        ctx: &mut Self::CTX,
    ) -> pingora::Result<Box<pingora::prelude::HttpPeer>> {
        let ctx = ctx.clone().unwrap();

        let address = ctx.address;
        let health_check_path = ctx.health_check.unwrap_or_default();

        let health_check_url = format!("{address}{health_check_path}");

        let resp = reqwest::get(health_check_url)
            .await
            .ok()
            .map(|r| r.status())
            .unwrap_or_else(|| StatusCode::SERVICE_UNAVAILABLE);

        if resp != StatusCode::OK {
            let cmd: Vec<_> = ctx.start.split_whitespace().collect();

            let _ = tokio::process::Command::new(cmd[0])
                .arg(cmd[1])
                .spawn()
                .expect("failed to spawn");
            tokio::time::sleep(Duration::from_secs(1)).await;
        }

        let peer = HttpPeer::new(address, false, "".to_owned());
        Ok(Box::new(peer))
    }
}

fn main() -> color_eyre::Result<()> {
    let Args {
        command: Command::Serve { config, address },
    } = Args::parse();

    let mut server = pingora::server::Server::new(None).unwrap();
    server.bootstrap();

    let config = std::fs::read_to_string(config)?;
    let config: Config = toml::from_str(&config)?;

    let proxy = YarpProxy::new(config);

    let mut proxy_service = pingora::prelude::http_proxy_service(&server.configuration, proxy);
    proxy_service.add_tcp(&address);

    server.add_service(proxy_service);

    println!("Starting proxy server on {}", address);
    server.run_forever();
}
