use std::collections::BTreeSet;
use std::fs;
use std::net::{IpAddr, Ipv4Addr};
#[cfg(target_family = "unix")]
use std::path::PathBuf;

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::de::{Error, Unexpected};
use serde::{Deserialize, Deserializer, Serialize};
use tokio::sync::OnceCell;

mod default {
    use super::*;

    pub fn instance_timeout() -> Option<u32> {
        // 5 minutes
        Some(5 * 60)
    }

    pub fn gc_interval() -> u32 {
        // 10 seconds
        10
    }

    pub fn listen() -> Address {
        // localhost & some random unprivileged port
        Address::Tcp(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 27_631)
    }

    pub fn connect() -> Address {
        listen()
    }

    pub fn log_filters() -> String {
        "info".to_owned()
    }

    pub fn log_mode() -> String {
        "terminal".to_owned()
    }

    pub fn pass_environment() -> BTreeSet<String> {
        BTreeSet::new()
    }
}

mod de {
    use super::*;

    /// parse either bool(false) or u32
    pub fn instance_timeout<'de, D>(deserializer: D) -> Result<Option<u32>, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum OneOf {
            Bool(bool),
            U32(u32),
        }

        match OneOf::deserialize(deserializer) {
            Ok(OneOf::U32(value)) => Ok(Some(value)),
            Ok(OneOf::Bool(false)) => Ok(None),
            Ok(OneOf::Bool(true)) => Err(Error::invalid_value(
                Unexpected::Bool(true),
                &"a non-negative integer or false",
            )),
            Err(_) => Err(Error::custom(
                "invalid type: expected a non-negative integer or false",
            )),
        }
    }

    /// make sure the value is greater than 0 to giver users feedback on invalid configuration
    pub fn gc_interval<'de, D>(deserializer: D) -> Result<u32, D::Error>
    where
        D: Deserializer<'de>,
    {
        match u32::deserialize(deserializer)? {
            0 => Err(Error::invalid_value(
                Unexpected::Unsigned(0),
                &"an integer 1 or greater",
            )),
            value => Ok(value),
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(untagged)]
pub enum Address {
    Tcp(IpAddr, u16),
    #[cfg(target_family = "unix")]
    Unix(PathBuf),
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default::instance_timeout")]
    #[serde(deserialize_with = "de::instance_timeout")]
    pub instance_timeout: Option<u32>,

    #[serde(default = "default::gc_interval")]
    #[serde(deserialize_with = "de::gc_interval")]
    pub gc_interval: u32,

    #[serde(default = "default::listen")]
    pub listen: Address,

    #[serde(default = "default::connect")]
    pub connect: Address,

    #[serde(default = "default::log_filters")]
    pub log_filters: String,

    #[serde(default = "default::log_mode")]
    pub log_mode: String,

    #[serde(default = "default::pass_environment")]
    pub pass_environment: BTreeSet<String>,
}

#[cfg(test)]
#[test]
fn generate_default_and_check_it_matches_commited_defaults() {
    use std::fs;
    use std::path::Path;

    let generated_defaults = Config::default();
    let generated_defaults = toml::to_string(&generated_defaults).expect("failed serialize");

    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("defaults.toml");
    let saved_defaults = fs::read_to_string(path).expect("failed reading defaults.toml file");

    assert_eq!(generated_defaults, saved_defaults);
}

impl Default for Config {
    fn default() -> Self {
        Config {
            instance_timeout: default::instance_timeout(),
            gc_interval: default::gc_interval(),
            listen: default::listen(),
            connect: default::connect(),
            log_filters: default::log_filters(),
            log_mode: default::log_mode(),
            pass_environment: default::pass_environment(),
        }
    }
}

impl Config {
    /// Try loading config file from the system default location
    pub fn try_load() -> Result<Self> {
        let pkg_name = env!("CARGO_PKG_NAME");
        let config_path = ProjectDirs::from("", "", pkg_name)
            .context("project config directory not found")?
            .config_dir()
            .join("config.toml");
        let path = config_path.display();
        let config_data =
            fs::read(&config_path).with_context(|| format!("cannot read config file `{path}`"))?;
        toml::from_slice(&config_data).with_context(|| format!("cannot parse config file `{path}`"))
    }

    /// Configure tracing-subscriber with env filter set to `log_filters` (if
    /// not overriden by RUST_LOG env var)
    ///
    /// Panics if called multiple times.
    pub async fn init_logger(&self) -> Result<()> {
        match self.log_mode.as_str() {
            "file" => self.init_file_logger().await,
            "terminal" | _ => self.init_terminal_logger(),
        }
    }

    fn init_terminal_logger(&self) -> Result<()> {
        use tracing_subscriber::prelude::*;
        use tracing_subscriber::EnvFilter;

        let format = tracing_subscriber::fmt::layer()
            .without_time()
            .with_target(false)
            .with_writer(std::io::stderr);

        let filter = EnvFilter::try_from_default_env()
            .or_else(|_| EnvFilter::try_new(&self.log_filters))
            .unwrap_or_else(|_| EnvFilter::new("info"));

        tracing_subscriber::registry()
            .with(filter)
            .with(format)
            .init();
        Ok(())
    }

    async fn init_file_logger(&self) -> Result<()> {
        use time::{format_description, UtcOffset};
        use tracing_subscriber::fmt::time::OffsetTime;
        use tracing_subscriber::prelude::*;
        use tracing_subscriber::EnvFilter;

        static FILE_LOGGER: OnceCell<Log> = OnceCell::const_new();

        let offset = UtcOffset::from_hms(8, 0, 0).unwrap();
        let _ = OffsetTime::new(offset, format_description::well_known::Rfc3339);

        let log = FILE_LOGGER
            .get_or_try_init(async || Self::init_file_writter().await)
            .await?;

        let format = tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_file(false)
            .with_line_number(false)
            .with_target(false)
            .compact()
            .with_writer(log.non_blocking.clone());

        let filter = EnvFilter::try_from_default_env()
            .or_else(|_| EnvFilter::try_new(&self.log_filters))
            .unwrap_or_else(|_| EnvFilter::new("info"));

        tracing_subscriber::registry()
            .with(filter)
            .with(format)
            .init();
        Ok(())
    }

    async fn init_file_writter() -> Result<Log> {
        let pkg_name = env!("CARGO_PKG_NAME");
        let log_file = ProjectDirs::from("", "", pkg_name)
            .context("project log path not found")?
            .cache_dir()
            .join("ra_multiplex.log");

        let attr = tokio::fs::metadata(&log_file).await;
        if attr.is_ok_and(|ref a| a.is_file() && a.len() >= 1048576) {
            tokio::fs::remove_file(&log_file).await?;
        }
        let dir = log_file.parent().context("invalid log path")?;
        let file = log_file.file_name().context("invalid log name")?;

        tokio::fs::create_dir_all(dir).await?;
        let file_appender = tracing_appender::rolling::never(dir, file);
        let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
        Ok(Log {
            non_blocking,
            _guard,
        })
    }
}

struct Log {
    non_blocking: tracing_appender::non_blocking::NonBlocking,
    _guard: tracing_appender::non_blocking::WorkerGuard,
}
