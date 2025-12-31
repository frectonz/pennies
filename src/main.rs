use clap::{Parser, Subcommand};
use jiff::SignedDuration;
use pingora::ErrorType::InvalidHTTPHeader;
use pingora::prelude::HttpPeer;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::str::FromStr;
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

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct App {
    pub address: SocketAddr,
    pub health_check: String,
    pub command: AppCommand,

    #[serde(default = "default_wait_period")]
    pub wait_period: SignedDuration,
    #[serde(default = "default_start_timeout")]
    pub start_timeout: SignedDuration,
}

fn default_wait_period() -> SignedDuration {
    SignedDuration::from_mins(10)
}

fn default_start_timeout() -> SignedDuration {
    SignedDuration::from_secs(30)
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(untagged)]
pub enum AppCommand {
    Start(CommandSpec),
    StartEnd {
        start: CommandSpec,
        end: CommandSpec,
    },
}

#[derive(Debug, Clone)]
pub struct CommandSpec {
    program: String,
    args: Vec<String>,
}

impl Serialize for CommandSpec {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let program = &self.program;
        let args = shell_words::join(self.args.as_slice());
        let command = format!("{program} {args}");

        serializer.serialize_str(&command)
    }
}

impl FromStr for CommandSpec {
    type Err = shell_words::ParseError;

    fn from_str(command: &str) -> Result<Self, Self::Err> {
        let mut words = shell_words::split(command)?;

        words.reverse();

        let program = words.pop().unwrap_or_else(|| command.to_owned());

        words.reverse();

        Ok(Self {
            program,
            args: words,
        })
    }
}

impl<'de> Deserialize<'de> for CommandSpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let command = String::deserialize(deserializer)?;
        CommandSpec::from_str(&command).map_err(serde::de::Error::custom)
    }
}

impl CommandSpec {
    fn to_command(&self) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(&self.program);
        command.args(&self.args);
        command
    }
}

impl AppCommand {
    fn get_start(&self) -> tokio::process::Command {
        match self {
            AppCommand::Start(s) => s.to_command(),
            AppCommand::StartEnd { start: _, end: _ } => todo!(),
        }
    }
}

impl App {
    async fn is_running(&self) -> bool {
        let address = self.address;
        let health_check_path = self.health_check.as_str();

        let health_check_url = format!("http://{address}{health_check_path}");

        let resp = reqwest::get(health_check_url)
            .await
            .ok()
            .map(|r| r.status())
            .unwrap_or_else(|| StatusCode::SERVICE_UNAVAILABLE);

        resp == StatusCode::OK
    }
}

#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(flatten)]
    pub apps: HashMap<String, App>,
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
    type CTX = Option<App>;

    fn new_ctx(&self) -> Option<App> {
        None
    }

    async fn early_request_filter(
        &self,
        session: &mut pingora::prelude::Session,
        ctx: &mut Self::CTX,
    ) -> pingora::Result<()> {
        dbg!(&session.req_header().uri.host());
        dbg!(session.get_header("host"));

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
        _session: &mut pingora::proxy::Session,
        ctx: &mut Self::CTX,
    ) -> pingora::Result<Box<pingora::prelude::HttpPeer>> {
        let ctx = ctx.clone().unwrap();

        if !ctx.is_running().await {
            let _ = ctx.command.get_start().spawn().expect("failed to spawn");
            tokio::time::sleep(Duration::from_secs(1)).await;
        }

        let peer = HttpPeer::new(ctx.address, false, "".to_owned());
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

    println!("Starting proxy server on {address}");
    server.run_forever()
}
