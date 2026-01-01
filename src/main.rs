use clap::{Parser, Subcommand};
use jiff::SignedDuration;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, error, info, instrument, warn};

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

#[derive(Debug, Deserialize, Serialize)]
pub struct App {
    pub address: SocketAddr,
    pub health_check: String,
    pub command: AppCommand,

    #[serde(default = "default_wait_period")]
    pub wait_period: SignedDuration,
    #[serde(default = "default_start_timeout")]
    pub start_timeout: SignedDuration,

    #[serde(skip)]
    pub kill_task: Option<tokio::task::JoinHandle<()>>,
}

fn default_wait_period() -> SignedDuration {
    SignedDuration::from_mins(10)
}

fn default_start_timeout() -> SignedDuration {
    SignedDuration::from_secs(30)
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum AppCommand {
    Start(Box<CommandSpec>),
    StartEnd {
        start: Box<CommandSpec>,
        end: Box<CommandSpec>,
    },
}

#[derive(Debug)]
pub struct CommandSpec {
    program: String,
    args: Vec<String>,

    child: Option<tokio::process::Child>,
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
            child: None,
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
    #[instrument(skip(self), fields(program = %self.program))]
    fn run(&mut self) {
        info!(args = ?self.args, "spawning command");
        let child = tokio::process::Command::new(&self.program)
            .args(&self.args)
            .spawn()
            .expect("failed to spawn command");

        self.child = Some(child);
        debug!("command spawned successfully");
    }

    #[instrument(skip(self), fields(program = %self.program))]
    async fn kill(&mut self) {
        if let Some(mut child) = self.child.take() {
            info!("killing process");
            child.kill().await.expect("failed to kill process");
            debug!("process killed successfully");
        } else {
            debug!("no child process to kill");
        }
    }
}

impl AppCommand {
    #[instrument(skip(self))]
    fn start(&mut self) {
        debug!("starting app command");
        let start = match self {
            AppCommand::Start(start) => start.as_mut(),
            AppCommand::StartEnd { start, end: _ } => start.as_mut(),
        };

        start.run();
    }

    #[instrument(skip(self))]
    async fn stop(&mut self) {
        debug!("stopping app command");
        match self {
            AppCommand::Start(start) => start.kill().await,
            AppCommand::StartEnd { start: _, end } => end.kill().await,
        };
    }
}

impl App {
    #[instrument(skip(self), fields(address = %self.address, health_check = %self.health_check))]
    async fn is_running(&self) -> bool {
        let address = self.address;
        let health_check_path = self.health_check.as_str();

        let health_check_url = format!("http://{address}{health_check_path}");

        debug!(url = %health_check_url, "performing health check");

        let resp = reqwest::get(&health_check_url)
            .await
            .ok()
            .map(|r| r.status())
            .unwrap_or_else(|| http::StatusCode::SERVICE_UNAVAILABLE);

        let is_ok = resp == http::StatusCode::OK;
        debug!(status = %resp, is_running = is_ok, "health check result");

        is_ok
    }

    #[instrument(skip(self), fields(timeout = ?self.start_timeout))]
    async fn wait_for_running(&self) -> Result<(), tokio::time::error::Elapsed> {
        debug!("waiting for app to become ready");
        let wait_for_running = async {
            loop {
                if self.is_running().await {
                    break;
                }
            }
        };

        let result =
            tokio::time::timeout(self.start_timeout.unsigned_abs(), wait_for_running).await;
        if result.is_ok() {
            info!("app is now running");
        } else {
            warn!("timed out waiting for app to start");
        }
        result
    }

    #[instrument(skip(app))]
    async fn start_app(app: &Arc<RwLock<App>>) -> pingora::Result<()> {
        let app = app.clone();

        let mut app = app.write().await;

        if !app.is_running().await {
            info!(address = %app.address, "app not running, starting it");
            app.command.start();

            if app.wait_for_running().await.is_err() {
                error!("failed to start app within timeout, stopping");
                app.command.stop().await;
                return Err(pingora::Error::explain(
                    pingora::ErrorType::ConnectError,
                    "failed to start app",
                ));
            }
        } else {
            debug!(address = %app.address, "app already running");
        }

        Ok(())
    }

    #[instrument(skip(app))]
    async fn schedule_kill(app: &Arc<RwLock<App>>) {
        let app = app.clone();

        if let Some(task) = app.write().await.kill_task.take() {
            debug!("aborting previous kill task");
            task.abort();
        }

        let wait_period = app.read().await.wait_period;
        info!(wait_period = ?wait_period, "scheduling app shutdown");

        let handle = {
            let app = app.clone();
            tokio::spawn(async move {
                let wait_period = app.read().await.wait_period.unsigned_abs();
                tokio::time::sleep(wait_period).await;
                info!("wait period elapsed, stopping app");
                app.write().await.command.stop().await;
            })
        };

        app.write().await.kill_task = Some(handle);
    }
}

fn deserialize_apps<'de, D>(deserializer: D) -> Result<HashMap<String, Arc<RwLock<App>>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = HashMap::<String, App>::deserialize(deserializer)?;

    Ok(raw
        .into_iter()
        .map(|(k, v)| (k, Arc::new(RwLock::new(v))))
        .collect())
}

#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(flatten, deserialize_with = "deserialize_apps")]
    pub apps: HashMap<String, Arc<RwLock<App>>>,
}

impl Config {
    fn get_proxy_context(&self, host: &str) -> Option<ProxyContext> {
        self.apps
            .get(host)
            .cloned()
            .map(|app| ProxyContext::new(host, app))
    }
}

pub struct YarpProxy {
    config: Config,
}

impl YarpProxy {
    fn new(config: Config) -> Self {
        Self { config }
    }
}

fn get_host(session: &pingora::prelude::Session) -> Option<&str> {
    session
        .get_header(http::header::HOST)
        .and_then(|value| value.to_str().ok())
        .or(session.req_header().uri.host())
}

pub struct ProxyContext {
    host: String,
    app: Arc<RwLock<App>>,
}

impl ProxyContext {
    fn new(host: &str, app: Arc<RwLock<App>>) -> Self {
        Self {
            host: host.to_owned(),
            app,
        }
    }
}

#[async_trait::async_trait]
impl pingora::prelude::ProxyHttp for YarpProxy {
    type CTX = Option<ProxyContext>;

    fn new_ctx(&self) -> Self::CTX {
        None
    }

    async fn early_request_filter(
        &self,
        session: &mut pingora::prelude::Session,
        ctx: &mut Self::CTX,
    ) -> pingora::Result<()> {
        let host = get_host(session).ok_or_else(|| {
            warn!("request missing host header");
            pingora::Error::explain(pingora::ErrorType::InvalidHTTPHeader, "failed to get host")
        })?;

        debug!(host = %host, "processing request");
        *ctx = self.config.get_proxy_context(host);

        if ctx.is_none() {
            warn!(host = %host, "no app configured for host");
        }

        Ok(())
    }

    async fn upstream_peer(
        &self,
        _session: &mut pingora::proxy::Session,
        ctx: &mut Self::CTX,
    ) -> pingora::Result<Box<pingora::prelude::HttpPeer>> {
        let ctx = ctx.take().ok_or_else(|| {
            error!("no proxy context available");
            pingora::Error::explain(
                pingora::ErrorType::ConnectError,
                "failed to get proxy context",
            )
        })?;

        info!(host = %ctx.host, "proxying request");

        App::start_app(&ctx.app).await?;
        App::schedule_kill(&ctx.app).await;

        let address = ctx.app.read().await.address;
        debug!(host = %ctx.host, upstream = %address, "connecting to upstream");

        let peer = pingora::prelude::HttpPeer::new(address, false, ctx.host);
        Ok(Box::new(peer))
    }
}

fn main() -> color_eyre::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    let Args {
        command: Command::Serve { config, address },
    } = Args::parse();

    info!(config = %config, address = %address, "starting pennies proxy");

    let mut server = pingora::server::Server::new(None).unwrap();
    server.bootstrap();

    let config_content = std::fs::read_to_string(&config)?;
    let config: Config = toml::from_str(&config_content)?;

    info!(apps_count = config.apps.len(), "loaded configuration");
    for (host, app) in &config.apps {
        let app = app.blocking_read();
        info!(
            host = %host,
            address = %app.address,
            health_check = %app.health_check,
            "registered app"
        );
    }

    let proxy = YarpProxy::new(config);

    let mut proxy_service = pingora::prelude::http_proxy_service(&server.configuration, proxy);
    proxy_service.add_tcp(&address);

    server.add_service(proxy_service);

    info!(address = %address, "proxy server listening");
    server.run_forever()
}
