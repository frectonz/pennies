use clap::{Parser, Subcommand};
use jiff::SignedDuration;
use serde::{Deserialize, Serialize};
use std::fmt::Debug;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::{collections::HashMap, time::Duration};
use tokio::sync::RwLock;
use tracing::{debug, error, info, instrument, warn};

static HTTP: once_cell::sync::Lazy<reqwest::Client> =
    once_cell::sync::Lazy::new(reqwest::Client::new);

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
    #[serde(default = "default_stop_timeout")]
    pub stop_timeout: SignedDuration,

    #[serde(skip)]
    pub kill_task: Option<tokio::task::JoinHandle<()>>,
}

fn default_wait_period() -> SignedDuration {
    SignedDuration::from_mins(10)
}

fn default_start_timeout() -> SignedDuration {
    SignedDuration::from_secs(30)
}

fn default_stop_timeout() -> SignedDuration {
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

#[derive(Debug)]
#[allow(dead_code)]
struct RunOptions<C: Collector> {
    run_id: RunId,
    collector: C,
}

impl CommandSpec {
    fn is_child_running(&mut self) -> bool {
        match self.child.as_mut() {
            Some(child) => match child.try_wait() {
                Ok(Some(_)) => false,
                Ok(None) => true,
                Err(_) => false,
            },
            None => false,
        }
    }

    #[instrument(skip(self), fields(program = %self.program))]
    fn run<C: Collector>(&mut self, opts: Option<RunOptions<C>>) {
        let should_spawn = match self.child.as_mut() {
            Some(child) => match child.try_wait() {
                Ok(Some(exit)) => {
                    warn!("child process exited with code {exit}, need to spawn new one");
                    true
                }
                Ok(None) => {
                    debug!("child process already exists, skipping spawn");
                    false
                }
                Err(err) => {
                    error!("failed to wait for child process: {err}");
                    true
                }
            },
            None => true,
        };

        if !should_spawn {
            return;
        };

        info!(args = ?self.args, "spawning command");
        match tokio::process::Command::new(&self.program)
            .args(&self.args)
            .spawn()
        {
            Ok(child) => {
                self.child = Some(child);
                debug!("command spawned successfully");
            }
            Err(err) => {
                error!("failed to spawn command: {err}");
            }
        };
    }

    #[instrument(skip(self), fields(program = %self.program))]
    async fn kill(&mut self) {
        if let Some(mut child) = self.child.take() {
            info!("killing process");

            match child.kill().await {
                Ok(()) => {
                    debug!("process killed successfully");
                }
                Err(err) => {
                    error!("failed to kill process: {err}");
                }
            };
        } else {
            debug!("no child process to kill");
        }
    }
}

impl AppCommand {
    fn is_child_running(&mut self) -> bool {
        match self {
            AppCommand::Start(start) => start.is_child_running(),
            AppCommand::StartEnd { start, .. } => start.is_child_running(),
        }
    }

    #[instrument(skip(self))]
    fn start<C: Collector>(&mut self, opts: Option<RunOptions<C>>) {
        debug!("starting app command");
        let start = match self {
            AppCommand::Start(start) => start.as_mut(),
            AppCommand::StartEnd { start, end: _ } => start.as_mut(),
        };

        start.run(opts);
    }

    #[instrument(skip(self))]
    async fn stop(&mut self) {
        debug!("stopping app command");
        match self {
            AppCommand::Start(start) => start.kill().await,
            AppCommand::StartEnd { start, end } => {
                start.kill().await;
                end.run::<NoOpCollector>(None)
            }
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

        let resp = HTTP
            .get(&health_check_url)
            .send()
            .await
            .ok()
            .map(|r| r.status())
            .unwrap_or_else(|| http::StatusCode::SERVICE_UNAVAILABLE);

        let is_ok = resp == http::StatusCode::OK;
        debug!(status = %resp, is_running = is_ok, "health check result");

        is_ok
    }

    #[instrument(skip(self), fields(timeout = ?self.start_timeout))]
    async fn wait_for_running(&self) -> Result<(), pingora::time::Elapsed> {
        let strategy = tokio_retry::strategy::ExponentialBackoff::from_millis(10)
            .max_delay(Duration::from_secs(2))
            .map(tokio_retry::strategy::jitter);

        debug!("waiting for app to become ready");
        let wait_for_running = tokio_retry::Retry::spawn(strategy, async || -> Result<(), ()> {
            if self.is_running().await {
                Ok(())
            } else {
                Err(())
            }
        });

        let result = pingora::time::timeout(self.start_timeout.unsigned_abs(), wait_for_running)
            .await
            .map(|_| ());
        if result.is_ok() {
            info!("app is now running");
        } else {
            warn!("timed out waiting for app to start");
        }

        result
    }

    #[instrument(skip(self), fields(timeout = ?self.start_timeout))]
    async fn wait_for_stopped(&self) -> Result<(), pingora::time::Elapsed> {
        let strategy = tokio_retry::strategy::ExponentialBackoff::from_millis(10)
            .max_delay(Duration::from_secs(2))
            .map(tokio_retry::strategy::jitter);

        debug!("waiting for app to stop");
        let wait_for_stopping = tokio_retry::Retry::spawn(strategy, async || -> Result<(), ()> {
            if self.is_running().await {
                Err(())
            } else {
                Ok(())
            }
        });

        let result = pingora::time::timeout(self.stop_timeout.unsigned_abs(), wait_for_stopping)
            .await
            .map(|_| ());
        if result.is_ok() {
            info!("app is now stopped");
        } else {
            warn!("timed out waiting for app to stop");
        }

        result
    }

    #[instrument(skip(app))]
    async fn start_app(
        host: &Host,
        app: &Arc<RwLock<App>>,
        collector: impl Collector,
    ) -> pingora::Result<()> {
        // Fast path: if child process is already running, skip health check
        if app.write().await.command.is_child_running() {
            debug!("child process already running, skipping health check");
            return Ok(());
        }

        // Slow path: no running child, do health check to confirm app state
        if !app.read().await.is_running().await {
            let address = app.read().await.address;
            let run_id = collector.app_started(host).await;

            info!(%address, "app not running, starting it");
            app.write()
                .await
                .command
                .start(Some(RunOptions { run_id, collector }));

            if app.read().await.wait_for_running().await.is_err() {
                error!("failed to start app within timeout");
                return Err(pingora::Error::explain(
                    pingora::ErrorType::ConnectError,
                    "failed to start app",
                ));
            }
        } else {
            let address = app.read().await.address;
            debug!(%address, "app already running");
        }

        Ok(())
    }

    #[instrument(skip(app))]
    async fn schedule_kill(host: &Host, app: &Arc<RwLock<App>>, collector: impl Collector) {
        let mut app_guard = app.write().await;

        if let Some(task) = app_guard.kill_task.take() {
            debug!("aborting previous kill task");
            task.abort();
        }

        let wait_period = app_guard.wait_period;
        info!(wait_period = ?wait_period, "scheduling app shutdown");

        let handle = {
            let app = app.clone();
            tokio::spawn(async move {
                let wait_period = app.read().await.wait_period.unsigned_abs();
                pingora::time::sleep(wait_period).await;
                info!("wait period elapsed, stopping app");
                app.write().await.command.stop().await;

                if app.read().await.wait_for_stopped().await.is_err() {
                    error!("failed to stop app within timeout");
                }
            })
        };

        app_guard.kill_task = Some(handle);
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
    async fn get_proxy_context(&self, host: &str) -> Option<ProxyContext> {
        let app = self.apps.get(host)?.clone();
        let proxy_context = ProxyContext::new(host, app).await;
        Some(proxy_context)
    }
}

struct YarpProxy<C> {
    config: Config,
    collector: C,
}

impl<C> YarpProxy<C>
where
    C: Collector,
{
    fn new(config: Config, collector: C) -> Self {
        Self { config, collector }
    }
}

fn get_host(session: &pingora::prelude::Session) -> Option<&str> {
    session
        .get_header(http::header::HOST)
        .and_then(|value| value.to_str().ok())
        .and_then(|host| host.split(':').next())
        .or(session.req_header().uri.host())
}

#[derive(Debug)]
struct Host(String);

#[derive(Debug)]
#[allow(dead_code)]
struct RunId(String);

impl std::fmt::Display for Host {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[allow(dead_code)]
#[async_trait::async_trait]
trait Collector: Sync + Send + Clone + Debug {
    async fn app_started(&self, host: &Host) -> RunId;
    async fn app_stopped(&self, host: &Host);

    async fn app_start_failed(&self, host: &Host);
    async fn app_stop_failed(&self, host: &Host);

    async fn append_stdout(&self, run_id: &RunId, data: &[u8]);
    async fn append_stderr(&self, run_id: &RunId, data: &[u8]);
}

#[derive(Debug, Clone)]
struct NoOpCollector;

#[allow(dead_code)]
#[allow(unused_variables)]
#[async_trait::async_trait]
impl Collector for NoOpCollector {
    async fn app_started(&self, host: &Host) -> RunId {
        unimplemented!()
    }

    async fn app_stopped(&self, host: &Host) {
        unimplemented!()
    }

    async fn app_start_failed(&self, host: &Host) {
        unimplemented!()
    }

    async fn app_stop_failed(&self, host: &Host) {
        unimplemented!()
    }

    async fn append_stdout(&self, run_id: &RunId, data: &[u8]) {
        unimplemented!()
    }

    async fn append_stderr(&self, run_id: &RunId, data: &[u8]) {
        unimplemented!()
    }
}

pub struct ProxyContext {
    host: Host,
    app: Arc<RwLock<App>>,
    peer: Box<pingora::prelude::HttpPeer>,
}

impl ProxyContext {
    async fn new(host: &str, app: Arc<RwLock<App>>) -> Self {
        let address = app.read().await.address;

        Self {
            app,
            host: Host(host.to_owned()),
            peer: Box::new(pingora::prelude::HttpPeer::new(
                address,
                false,
                host.to_owned(),
            )),
        }
    }
}

#[async_trait::async_trait]
impl<C> pingora::prelude::ProxyHttp for YarpProxy<C>
where
    C: Collector,
{
    type CTX = Option<ProxyContext>;

    fn new_ctx(&self) -> Self::CTX {
        None
    }

    async fn request_filter(
        &self,
        session: &mut pingora::prelude::Session,
        ctx: &mut Self::CTX,
    ) -> pingora::Result<bool> {
        let host = get_host(session).ok_or_else(|| {
            warn!("request missing host header");
            pingora::Error::explain(pingora::ErrorType::InvalidHTTPHeader, "failed to get host")
        })?;

        debug!(host = %host, "processing request");
        *ctx = self.config.get_proxy_context(host).await;

        if ctx.is_none() {
            warn!(host = %host, "no app configured for host");
        }

        Ok(false)
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

        App::start_app(&ctx.host, &ctx.app, self.collector.clone()).await?;
        App::schedule_kill(&ctx.host, &ctx.app, self.collector.clone()).await;

        let address = ctx.app.read().await.address;
        debug!(host = %ctx.host, upstream = %address, "connecting to upstream");

        Ok(ctx.peer.clone())
    }
}

fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;

    let filter =
        std::env::var("RUST_LOG").unwrap_or_else(|_| "tracing=info,penny=debug".to_owned());
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
        .init();

    let Args {
        command: Command::Serve { config, address },
    } = Args::parse();

    info!(config = %config, address = %address, "starting penny proxy");

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

    let proxy = YarpProxy::new(config, NoOpCollector);

    let mut proxy_service = pingora::prelude::http_proxy_service(&server.configuration, proxy);
    proxy_service.add_tcp(&address);

    server.add_service(proxy_service);

    info!(address = %address, "proxy server listening");
    server.run_forever()
}
