use std::{
    collections::BTreeMap,
    env,
    fmt::{Debug, Display, Formatter},
    io::ErrorKind,
    net::SocketAddr,
    str::FromStr,
    thread,
    time::Duration,
};

use vela_relay_core::chain_directory::ChainDirectory;

/// Default in-band reimbursement multiplier: 1.4x the simulated outer transaction cost.
pub const DEFAULT_SETTLEMENT_MARKUP_BPS: u64 = 14_000;

/// Lowest outer fee cap the executor will sign for, as basis points of the latest base fee (the
/// tip is added on top). The normal cap is 2x base fee — headroom for inclusion, not real cost,
/// since a transaction only ever pays `base fee + tip`. When a payer's signed reimbursement
/// cannot fund the 2x cap the executor may reprice down to what they did pay, but never below
/// this floor: the relay cannot bump a signed transaction (the outbox broadcasts exact bytes), so
/// an underpriced transaction would wedge its lane's nonce. 1.5x survives three consecutive
/// blocks of maximum EIP-1559 base-fee growth (1.125^3 ≈ 1.42).
pub const DEFAULT_SETTLEMENT_INCLUSION_FLOOR_BPS: u64 = 15_000;

/// How many times a UserOperation may be held for an unaffordable market before it is rejected.
/// The delayed inbox doubles 5s → 5min and then holds there (5+10+20+40+80+160, then 300 each), so
/// 12 attempts spend ~35 minutes waiting before the 13th evaluation rejects. Long enough to ride
/// out ordinary volatility, and short enough to stay inside the 1-hour status-record TTL so the
/// wallet can still read the outcome rather than getting `not_found`.
pub const DEFAULT_SETTLEMENT_HOLD_MAX_ATTEMPTS: u32 = 12;

/// Amount retained for the next treasury transfer without preventing a lightly funded chain from
/// serving the UserOperation that caused a relayer top-up.
pub const DEFAULT_TREASURY_FLOOR_WEI: u128 = 100_000_000_000_000;

#[derive(Clone, Debug)]
pub struct Config {
    pub listen_addr: SocketAddr,
    pub logging: LoggingConfig,
    pub http: HttpConfig,
    pub runtime: RuntimeConfig,
    pub worker: WorkerConfig,
    pub iggy: IggyConfig,
    pub redis: RedisConfig,
    pub executor: ExecutorConfig,
    pub settlement_recipient: Option<String>,
    /// Where chain metadata (RPC endpoints, native asset, stablecoins) is read from.
    pub chain_directory: ChainDirectory,
}

#[derive(Clone, Debug)]
pub struct LoggingConfig {
    pub filter: String,
    pub format: LogFormat,
}

#[derive(Clone, Copy, Debug)]
pub enum LogFormat {
    Pretty,
    Json,
}

#[derive(Clone, Debug)]
pub struct HttpConfig {
    pub request_timeout: Duration,
    pub max_body_bytes: usize,
    pub max_concurrency: usize,
}

#[derive(Clone, Debug)]
pub struct RuntimeConfig {
    pub http_worker_threads: usize,
    pub http_max_blocking_threads: usize,
}

#[derive(Clone, Debug)]
pub struct WorkerConfig {
    pub runtime_threads: usize,
    pub max_blocking_threads: usize,
    pub parallel_job_concurrency: usize,
}

#[derive(Clone, Debug)]
pub struct IggyConfig {
    /// An Iggy connection string. Credentials must be supplied through the environment, never
    /// committed in configuration files.
    pub url: String,
    /// Separately privileged consumer credentials. Defaults to `url` for local/backwards-
    /// compatible deployments, but production should grant only list/poll/group/offset access.
    pub consumer_url: String,
    /// Iggy credentials used to create a chain stream and topic when they are first used.
    /// Defaults to `url`; deployments that split privileges can override it.
    pub provisioner_url: String,
    pub topic: String,
    pub enqueue_timeout: Duration,
}

#[derive(Clone, Debug)]
pub struct RedisConfig {
    /// Redis connection string for durable UserOperation status. It is mandatory: the relay does
    /// not acknowledge a UserOperation unless both Redis and Iggy accept it.
    pub url: String,
    pub command_timeout: Duration,
}

#[derive(Clone)]
pub struct ExecutorConfig {
    pub enabled: bool,
    pub operator_secret: SecretString,
    pub alchemy_api_key: Option<SecretString>,
    pub trusted_rpc_urls: BTreeMap<u64, Vec<String>>,
    pub consumer_group_prefix: String,
    pub pool_width: usize,
    pub stream_discovery_interval: Duration,
    pub idle_poll_interval: Duration,
    pub poll_batch_size: u32,
    pub max_bundle_operations: usize,
    pub rpc_timeout: Duration,
    pub lease_ttl: Duration,
    pub receipt_poll_interval: Duration,
    pub receipt_confirmations: u64,
    pub attempt_ttl: Duration,
    pub gas_buffer_bps: u64,
    pub fixed_gas_buffer: u64,
    pub settlement_markup_bps: u64,
    pub settlement_inclusion_floor_bps: u64,
    pub settlement_hold_max_attempts: u32,
    pub relayer_float_min_wei: u128,
    pub relayer_float_target_wei: u128,
    pub relayer_float_cost_multiplier: u64,
    pub treasury_floor_wei: u128,
    pub top_up_max_wei: u128,
    pub telegram_alerts: TelegramAlertsConfig,
}

/// Optional Telegram delivery for executor failures. Both credentials must be set together so a
/// partially configured deployment never silently drops alerts.
#[derive(Clone)]
pub struct TelegramAlertsConfig {
    pub bot_token: Option<SecretString>,
    pub chat_id: Option<String>,
    pub cooldown: Duration,
}

impl TelegramAlertsConfig {
    pub fn is_enabled(&self) -> bool {
        self.bot_token.is_some() && self.chat_id.is_some()
    }
}

#[derive(Clone)]
pub struct SecretString(String);

impl SecretString {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl Debug for SecretString {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("<redacted>")
    }
}

impl Debug for ExecutorConfig {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ExecutorConfig")
            .field("enabled", &self.enabled)
            .field("operator_secret", &self.operator_secret)
            .field("alchemy_api_key", &self.alchemy_api_key)
            .field(
                "trusted_rpc_chain_ids",
                &self.trusted_rpc_urls.keys().collect::<Vec<_>>(),
            )
            .field("consumer_group_prefix", &self.consumer_group_prefix)
            .field("pool_width", &self.pool_width)
            .field("stream_discovery_interval", &self.stream_discovery_interval)
            .field("idle_poll_interval", &self.idle_poll_interval)
            .field("poll_batch_size", &self.poll_batch_size)
            .field("max_bundle_operations", &self.max_bundle_operations)
            .field("rpc_timeout", &self.rpc_timeout)
            .field("lease_ttl", &self.lease_ttl)
            .field("receipt_poll_interval", &self.receipt_poll_interval)
            .field("receipt_confirmations", &self.receipt_confirmations)
            .field("attempt_ttl", &self.attempt_ttl)
            .field("gas_buffer_bps", &self.gas_buffer_bps)
            .field("fixed_gas_buffer", &self.fixed_gas_buffer)
            .field("settlement_markup_bps", &self.settlement_markup_bps)
            .field("relayer_float_min_wei", &self.relayer_float_min_wei)
            .field("relayer_float_target_wei", &self.relayer_float_target_wei)
            .field(
                "relayer_float_cost_multiplier",
                &self.relayer_float_cost_multiplier,
            )
            .field("treasury_floor_wei", &self.treasury_floor_wei)
            .field("top_up_max_wei", &self.top_up_max_wei)
            .field(
                "telegram_alerts_enabled",
                &self.telegram_alerts.is_enabled(),
            )
            .field("telegram_alert_cooldown", &self.telegram_alerts.cooldown)
            .finish()
    }
}

#[derive(Debug)]
pub struct ConfigError(String);

impl Display for ConfigError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        load_dotenv()?;

        let available_cores = thread::available_parallelism()
            .map(|cores| cores.get())
            .unwrap_or(1);
        let parallel_job_concurrency = usize_value("VELA_RELAY_PARALLEL_JOB_CONCURRENCY", 10)?;
        let worker_max_blocking_threads =
            usize_value("VELA_RELAY_WORKER_MAX_BLOCKING_THREADS", 10)?;

        if parallel_job_concurrency > worker_max_blocking_threads {
            return Err(ConfigError(format!(
                "VELA_RELAY_PARALLEL_JOB_CONCURRENCY ({parallel_job_concurrency}) cannot exceed VELA_RELAY_WORKER_MAX_BLOCKING_THREADS ({worker_max_blocking_threads})"
            )));
        }

        Ok(Self {
            listen_addr: value_or("VELA_RELAY_LISTEN_ADDR", "0.0.0.0:4567")?
                .parse()
                .map_err(|error| ConfigError(format!("invalid VELA_RELAY_LISTEN_ADDR: {error}")))?,
            logging: LoggingConfig {
                filter: value_or("RUST_LOG", "vela_relay=info,tower_http=info")?,
                format: value_or("VELA_RELAY_LOG_FORMAT", "pretty")?.parse()?,
            },
            http: HttpConfig {
                request_timeout: Duration::from_secs(u64_value(
                    "VELA_RELAY_HTTP_REQUEST_TIMEOUT_SECS",
                    30,
                )?),
                max_body_bytes: usize_value("VELA_RELAY_HTTP_MAX_BODY_BYTES", 1_048_576)?,
                max_concurrency: usize_value("VELA_RELAY_HTTP_MAX_CONCURRENCY", 256)?,
            },
            runtime: RuntimeConfig {
                http_worker_threads: usize_value(
                    "VELA_RELAY_HTTP_WORKER_THREADS",
                    available_cores,
                )?,
                http_max_blocking_threads: usize_value("VELA_RELAY_HTTP_MAX_BLOCKING_THREADS", 16)?,
            },
            worker: WorkerConfig {
                runtime_threads: usize_value("VELA_RELAY_WORKER_THREADS", 1)?,
                max_blocking_threads: worker_max_blocking_threads,
                parallel_job_concurrency,
            },
            iggy: iggy_config()?,
            redis: RedisConfig {
                url: required_value("VELA_RELAY_REDIS_URL")?,
                command_timeout: Duration::from_secs(u64_value(
                    "VELA_RELAY_REDIS_COMMAND_TIMEOUT_SECS",
                    2,
                )?),
            },
            executor: executor_config()?,
            settlement_recipient: settlement_recipient()?,
            chain_directory: chain_directory()?,
        })
    }
}

fn chain_directory() -> Result<ChainDirectory, ConfigError> {
    let value = optional_value("VELA_RELAY_CHAIN_DIRECTORY_URL")?;
    ChainDirectory::from_setting(value.as_deref())
        .map_err(|error| ConfigError(format!("invalid VELA_RELAY_CHAIN_DIRECTORY_URL: {error}")))
}

fn executor_config() -> Result<ExecutorConfig, ConfigError> {
    let enabled = match optional_value("VELA_RELAY_EXECUTOR_ENABLED")? {
        Some(value) => parse_bool("VELA_RELAY_EXECUTOR_ENABLED", &value)?,
        None => match optional_value("VELA_RELAY_CONSUMER_ENABLED")? {
            Some(value) => parse_bool("VELA_RELAY_CONSUMER_ENABLED", &value)?,
            // The controlled chain directory supplies the default execution policy. Set the
            // primary flag to false for a producer-only relay.
            None => true,
        },
    };
    let operator_secret = match optional_value("OPERATOR_SECRET")? {
        Some(secret) => SecretString(secret),
        None if enabled => {
            return Err(ConfigError(
                "environment variable OPERATOR_SECRET is required when the UserOperation consumer is enabled"
                    .into(),
            ));
        }
        None => SecretString(String::new()),
    };
    let alchemy_api_key = optional_value("ALCHEMY_API_KEY")?.map(SecretString);
    let trusted_rpc_urls = match optional_value("VELA_RELAY_EXECUTOR_RPC_URLS")? {
        Some(value) => parse_trusted_rpc_urls(&value)?,
        None => BTreeMap::new(),
    };
    // The controlled chain directory provides trusted execution RPCs by default. Explicit URLs
    // and Alchemy remain optional, higher-priority overrides.
    let pool_width = usize_value("VELA_RELAY_RELAYER_COUNT", 10)?;
    if pool_width > crate::utils::vault::RELAYER_POOL_SIZE {
        return Err(ConfigError(format!(
            "VELA_RELAY_RELAYER_COUNT cannot exceed {}",
            crate::utils::vault::RELAYER_POOL_SIZE
        )));
    }
    if pool_width != crate::utils::vault::RELAYER_ROUTING_WIDTH {
        return Err(ConfigError(format!(
            "VELA_RELAY_RELAYER_COUNT must be {} while the Iggy queue uses fixed routing",
            crate::utils::vault::RELAYER_ROUTING_WIDTH
        )));
    }
    let settlement_markup_bps = u64_value(
        "VELA_RELAY_EXECUTOR_SETTLEMENT_MARKUP_BPS",
        DEFAULT_SETTLEMENT_MARKUP_BPS,
    )?;
    if settlement_markup_bps < 10_000 {
        return Err(ConfigError(
            "VELA_RELAY_EXECUTOR_SETTLEMENT_MARKUP_BPS cannot be below 10000".into(),
        ));
    }
    let settlement_inclusion_floor_bps = u64_value(
        "VELA_RELAY_EXECUTOR_SETTLEMENT_INCLUSION_FLOOR_BPS",
        DEFAULT_SETTLEMENT_INCLUSION_FLOOR_BPS,
    )?;
    // At or below the base fee the transaction is not includable at all, and the executor has no
    // fee-bump path to rescue it.
    if settlement_inclusion_floor_bps <= 10_000 {
        return Err(ConfigError(
            "VELA_RELAY_EXECUTOR_SETTLEMENT_INCLUSION_FLOOR_BPS must be above 10000".into(),
        ));
    }
    let settlement_hold_max_attempts = u32_value(
        "VELA_RELAY_EXECUTOR_SETTLEMENT_HOLD_MAX_ATTEMPTS",
        DEFAULT_SETTLEMENT_HOLD_MAX_ATTEMPTS,
    )?;
    let relayer_float_min_wei =
        u128_value("VELA_RELAY_EXECUTOR_FLOAT_MIN_WEI", 500_000_000_000_000)?;
    let relayer_float_target_wei = u128_value(
        "VELA_RELAY_EXECUTOR_FLOAT_TARGET_WEI",
        2_000_000_000_000_000,
    )?;
    if relayer_float_target_wei < relayer_float_min_wei {
        return Err(ConfigError(
            "VELA_RELAY_EXECUTOR_FLOAT_TARGET_WEI cannot be below VELA_RELAY_EXECUTOR_FLOAT_MIN_WEI"
                .into(),
        ));
    }
    let top_up_max_wei = u128_value(
        "VELA_RELAY_EXECUTOR_TOP_UP_MAX_WEI",
        10_000_000_000_000_000_000,
    )?;
    let receipt_confirmations = u64_value("VELA_RELAY_EXECUTOR_RECEIPT_CONFIRMATIONS", 2)?;
    if receipt_confirmations < 2 {
        return Err(ConfigError(
            "VELA_RELAY_EXECUTOR_RECEIPT_CONFIRMATIONS must be at least 2".into(),
        ));
    }
    let telegram_alerts = telegram_alerts_config()?;
    Ok(ExecutorConfig {
        enabled,
        operator_secret,
        alchemy_api_key,
        trusted_rpc_urls,
        consumer_group_prefix: value_or("VELA_RELAY_IGGY_CONSUMER_GROUP_PREFIX", "vela-relay-v1")?,
        pool_width,
        stream_discovery_interval: Duration::from_secs(u64_value(
            "VELA_RELAY_IGGY_STREAM_DISCOVERY_SECS",
            15,
        )?),
        idle_poll_interval: Duration::from_millis(u64_value(
            "VELA_RELAY_IGGY_IDLE_POLL_MILLIS",
            250,
        )?),
        poll_batch_size: u32_value("VELA_RELAY_IGGY_POLL_BATCH_SIZE", 100)?,
        max_bundle_operations: usize_value("VELA_RELAY_MAX_BUNDLE_OPERATIONS", 10)?,
        rpc_timeout: Duration::from_secs(u64_value("VELA_RELAY_EXECUTOR_RPC_TIMEOUT_SECS", 5)?),
        lease_ttl: Duration::from_secs(u64_value("VELA_RELAY_EXECUTOR_LEASE_TTL_SECS", 30)?),
        receipt_poll_interval: Duration::from_secs(u64_value(
            "VELA_RELAY_EXECUTOR_RECEIPT_POLL_SECS",
            3,
        )?),
        receipt_confirmations,
        attempt_ttl: Duration::from_secs(u64_value(
            "VELA_RELAY_EXECUTOR_ATTEMPT_TTL_SECS",
            48 * 60 * 60,
        )?),
        gas_buffer_bps: u64_value("VELA_RELAY_EXECUTOR_GAS_BUFFER_BPS", 1_500)?,
        fixed_gas_buffer: u64_value("VELA_RELAY_EXECUTOR_FIXED_GAS_BUFFER", 30_000)?,
        settlement_markup_bps,
        settlement_inclusion_floor_bps,
        settlement_hold_max_attempts,
        relayer_float_min_wei,
        relayer_float_target_wei,
        // Keep an unpriced chain's relayer float comfortably ahead of the immediate bundle
        // prefund without parking an excessive amount of an illiquid native asset.
        relayer_float_cost_multiplier: u64_value("VELA_RELAY_EXECUTOR_FLOAT_COST_MULTIPLIER", 5)?,
        treasury_floor_wei: u128_value(
            "VELA_RELAY_EXECUTOR_TREASURY_FLOOR_WEI",
            DEFAULT_TREASURY_FLOOR_WEI,
        )?,
        top_up_max_wei,
        telegram_alerts,
    })
}

fn telegram_alerts_config() -> Result<TelegramAlertsConfig, ConfigError> {
    let bot_token = optional_value("TELEGRAM_BOT_TOKEN")?.map(SecretString);
    let chat_id = optional_value("TELEGRAM_CHAT_ID")?;
    match (&bot_token, &chat_id) {
        (Some(_), Some(_)) | (None, None) => {}
        _ => {
            return Err(ConfigError(
                "TELEGRAM_BOT_TOKEN and TELEGRAM_CHAT_ID must be configured together".into(),
            ));
        }
    }

    Ok(TelegramAlertsConfig {
        bot_token,
        chat_id,
        cooldown: Duration::from_secs(u64_value(
            "VELA_RELAY_TELEGRAM_ALERT_COOLDOWN_SECS",
            30 * 60,
        )?),
    })
}

fn parse_trusted_rpc_urls(value: &str) -> Result<BTreeMap<u64, Vec<String>>, ConfigError> {
    let values = serde_json::from_str::<BTreeMap<String, serde_json::Value>>(value)
        .map_err(|error| ConfigError(format!("invalid VELA_RELAY_EXECUTOR_RPC_URLS: {error}")))?;
    let mut result = BTreeMap::new();

    for (chain_id, urls) in values {
        let chain_id = chain_id.parse::<u64>().map_err(|error| {
            ConfigError(format!(
                "invalid chain ID in VELA_RELAY_EXECUTOR_RPC_URLS: {error}"
            ))
        })?;
        let urls = match urls {
            serde_json::Value::String(url) => vec![url],
            serde_json::Value::Array(urls) => urls
                .into_iter()
                .map(|url| {
                    url.as_str().map(str::to_owned).ok_or_else(|| {
                        ConfigError(
                            "VELA_RELAY_EXECUTOR_RPC_URLS values must be URLs or URL arrays".into(),
                        )
                    })
                })
                .collect::<Result<Vec<_>, _>>()?,
            _ => {
                return Err(ConfigError(
                    "VELA_RELAY_EXECUTOR_RPC_URLS values must be URLs or URL arrays".into(),
                ));
            }
        };
        if urls.is_empty() {
            return Err(ConfigError(format!(
                "VELA_RELAY_EXECUTOR_RPC_URLS chain {chain_id} has no URL"
            )));
        }
        for url in &urls {
            let parsed = reqwest::Url::parse(url).map_err(|error| {
                ConfigError(format!(
                    "invalid RPC URL for chain {chain_id} in VELA_RELAY_EXECUTOR_RPC_URLS: {error}"
                ))
            })?;
            if !matches!(parsed.scheme(), "http" | "https") {
                return Err(ConfigError(format!(
                    "RPC URL for chain {chain_id} must use http or https"
                )));
            }
        }
        result.insert(chain_id, urls);
    }

    Ok(result)
}

fn iggy_config() -> Result<IggyConfig, ConfigError> {
    let url = required_value("VELA_RELAY_IGGY_URL")?;
    let consumer_url =
        optional_value("VELA_RELAY_IGGY_CONSUMER_URL")?.unwrap_or_else(|| url.clone());
    let provisioner_url =
        optional_value("VELA_RELAY_IGGY_PROVISIONER_URL")?.unwrap_or_else(|| url.clone());

    Ok(IggyConfig {
        url,
        consumer_url,
        provisioner_url,
        topic: value_or("VELA_RELAY_IGGY_TOPIC", "default")?,
        enqueue_timeout: Duration::from_secs(u64_value("VELA_RELAY_IGGY_ENQUEUE_TIMEOUT_SECS", 5)?),
    })
}

fn load_dotenv() -> Result<(), ConfigError> {
    match dotenvy::dotenv() {
        Ok(_) => Ok(()),
        Err(dotenvy::Error::Io(error)) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(ConfigError(format!("could not load .env: {error}"))),
    }
}

impl FromStr for LogFormat {
    type Err = ConfigError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "pretty" => Ok(Self::Pretty),
            "json" => Ok(Self::Json),
            _ => Err(ConfigError(format!(
                "invalid VELA_RELAY_LOG_FORMAT `{value}`; expected `pretty` or `json`"
            ))),
        }
    }
}

fn value_or(name: &str, default: &str) -> Result<String, ConfigError> {
    match env::var(name) {
        Ok(value) if value.trim().is_empty() => Err(ConfigError(format!(
            "environment variable {name} cannot be empty"
        ))),
        Ok(value) => Ok(value),
        Err(env::VarError::NotPresent) => Ok(default.into()),
        Err(env::VarError::NotUnicode(_)) => Err(ConfigError(format!(
            "environment variable {name} must be valid Unicode"
        ))),
    }
}

fn required_value(name: &str) -> Result<String, ConfigError> {
    match env::var(name) {
        Ok(value) if value.trim().is_empty() => Err(ConfigError(format!(
            "environment variable {name} cannot be empty"
        ))),
        Ok(value) => Ok(value),
        Err(env::VarError::NotPresent) => Err(ConfigError(format!(
            "environment variable {name} is required"
        ))),
        Err(env::VarError::NotUnicode(_)) => Err(ConfigError(format!(
            "environment variable {name} must be valid Unicode"
        ))),
    }
}

fn optional_value(name: &str) -> Result<Option<String>, ConfigError> {
    match env::var(name) {
        Ok(value) if value.trim().is_empty() => Err(ConfigError(format!(
            "environment variable {name} cannot be empty"
        ))),
        Ok(value) => Ok(Some(value)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => Err(ConfigError(format!(
            "environment variable {name} must be valid Unicode"
        ))),
    }
}

fn usize_value(name: &str, default: usize) -> Result<usize, ConfigError> {
    let value = value_or(name, &default.to_string())?;
    let parsed = value
        .parse::<usize>()
        .map_err(|error| ConfigError(format!("invalid {name}: {error}")))?;

    if parsed == 0 {
        return Err(ConfigError(format!(
            "environment variable {name} must be greater than zero"
        )));
    }

    Ok(parsed)
}

fn u32_value(name: &str, default: u32) -> Result<u32, ConfigError> {
    let parsed = u64_value(name, u64::from(default))?;
    parsed
        .try_into()
        .map_err(|_| ConfigError(format!("environment variable {name} is too large")))
}

fn parse_bool(name: &str, value: &str) -> Result<bool, ConfigError> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        _ => Err(ConfigError(format!(
            "invalid {name}; expected true or false"
        ))),
    }
}

fn u128_value(name: &str, default: u128) -> Result<u128, ConfigError> {
    let value = value_or(name, &default.to_string())?;
    let parsed = value
        .parse::<u128>()
        .map_err(|error| ConfigError(format!("invalid {name}: {error}")))?;

    if parsed == 0 {
        return Err(ConfigError(format!(
            "environment variable {name} must be greater than zero"
        )));
    }

    Ok(parsed)
}

fn u64_value(name: &str, default: u64) -> Result<u64, ConfigError> {
    let value = value_or(name, &default.to_string())?;
    let parsed = value
        .parse::<u64>()
        .map_err(|error| ConfigError(format!("invalid {name}: {error}")))?;

    if parsed == 0 {
        return Err(ConfigError(format!(
            "environment variable {name} must be greater than zero"
        )));
    }

    Ok(parsed)
}

fn optional_address(name: &str) -> Result<Option<String>, ConfigError> {
    let Some(value) = optional_value(name)? else {
        return Ok(None);
    };

    let address = value.trim();
    if validate_address(address).is_err() {
        return Err(ConfigError(format!(
            "invalid {name}; expected a 0x-prefixed 20-byte address"
        )));
    }

    Ok(Some(address.into()))
}

fn validate_address(address: &str) -> Result<(), ()> {
    (address.len() == 42
        && address.starts_with("0x")
        && address[2..].bytes().all(|byte| byte.is_ascii_hexdigit()))
    .then_some(())
    .ok_or(())
}

fn settlement_recipient() -> Result<Option<String>, ConfigError> {
    let configured_recipient = optional_address("VELA_RELAY_SETTLEMENT_RECIPIENT")?;
    let operator_secret = match env::var("OPERATOR_SECRET") {
        Ok(secret) => secret,
        Err(env::VarError::NotPresent) => return Ok(configured_recipient),
        Err(env::VarError::NotUnicode(_)) => {
            return Err(ConfigError(
                "environment variable OPERATOR_SECRET must be valid Unicode".into(),
            ));
        }
    };

    let derived_recipient = crate::utils::vault::derive_address(&operator_secret)
        .map_err(|error| ConfigError(format!("invalid OPERATOR_SECRET: {error}")))?;

    if let Some(configured_recipient) = configured_recipient
        && !configured_recipient.eq_ignore_ascii_case(&derived_recipient)
    {
        return Err(ConfigError(
            "VELA_RELAY_SETTLEMENT_RECIPIENT does not match the address derived from OPERATOR_SECRET"
                .into(),
        ));
    }

    Ok(Some(derived_recipient))
}
