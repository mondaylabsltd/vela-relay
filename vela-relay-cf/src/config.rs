//! Env/secrets → validated policy values, injected into the core as data
//! (Constitution II). Bounds mirror the docker parser where they apply; the
//! Iggy fixed-routing width pin deliberately does NOT (research.md R11).

use vela_relay_core::{chain_directory::ChainDirectory, vault};
use worker::Env;

#[derive(Clone)]
pub struct CfConfig {
    pub settlement_recipient: Option<String>,
    pub relayer_count: usize,
    pub executor_enabled: bool,
    /// Empty = dynamic directory-driven chains (research.md R10).
    pub execution_chains: Vec<u64>,
    pub alchemy_api_key: Option<String>,
    pub operator_secret: Option<String>,
    /// Explicit per-chain executor RPC endpoints (docker
    /// `VELA_RELAY_EXECUTOR_RPC_URLS`), tried before Alchemy and the directory.
    pub trusted_rpc_urls: std::collections::BTreeMap<u64, Vec<String>>,
    /// Per-request executor RPC deadline (docker `rpc_timeout`, default 5 s).
    pub rpc_timeout_ms: u64,
    /// Treasury lease TTL (docker `lease_ttl`, default 30 s).
    pub lease_ttl_ms: u64,
    /// Receipt-probe throttle interval (docker `receipt_poll_interval`,
    /// default 3 s).
    pub receipt_poll_ms: u64,
    /// Delayed-payload retention floor (docker `attempt_ttl`, default 48 h;
    /// the effective retention is `max(this, 14 d)` — queue-retention parity).
    pub attempt_ttl_ms: u64,
    /// Telegram alerts (docker `TelegramAlertsConfig`): both or neither.
    pub telegram_bot_token: Option<String>,
    pub telegram_chat_id: Option<String>,
    /// Suppression cooldown (docker default 30 min).
    pub telegram_cooldown_ms: u64,
    // Executor policy values — same names, defaults, and bounds as the docker
    // parser; injected into the core as data.
    pub max_bundle_operations: usize,
    pub gas_buffer_bps: u64,
    pub fixed_gas_buffer: u64,
    pub settlement_markup_bps: u64,
    pub settlement_inclusion_floor_bps: u64,
    pub settlement_hold_max_attempts: u32,
    pub relayer_float_cost_multiplier: u64,
    pub relayer_float_target_wei: u128,
    pub relayer_float_min_wei: u128,
    pub top_up_max_wei: u128,
    pub treasury_floor_wei: u128,
}

impl CfConfig {
    /// The core's per-batch policy, with the treasury and per-lane relayer
    /// addresses derived from `OPERATOR_SECRET` (core vault, chain-agnostic).
    pub fn execution_policy(
        &self,
        chain_id: u64,
        lane: u8,
    ) -> Result<vela_relay_core::execution::ExecutionPolicy, String> {
        let secret = self
            .operator_secret
            .as_deref()
            .ok_or("OPERATOR_SECRET is required for execution")?;
        let treasury = vault::derive_address(secret)
            .map_err(|error| format!("invalid OPERATOR_SECRET: {error}"))?
            .parse()
            .map_err(|_| "derived treasury address is invalid".to_owned())?;
        let relayer = vault::derive_pool_relayer_address(secret, lane as usize)
            .map_err(|error| format!("invalid OPERATOR_SECRET: {error}"))?
            .parse()
            .map_err(|_| "derived relayer address is invalid".to_owned())?;
        Ok(vela_relay_core::execution::ExecutionPolicy {
            pool_width: self.relayer_count,
            max_bundle_operations: self.max_bundle_operations,
            gas_buffer_bps: self.gas_buffer_bps,
            fixed_gas_buffer: self.fixed_gas_buffer,
            settlement_inclusion_floor_bps: self.settlement_inclusion_floor_bps,
            settlement_hold_max_attempts: self.settlement_hold_max_attempts,
            top_up_max_wei: self.top_up_max_wei,
            is_tempo: vela_relay_core::tempo::is_tempo_chain(chain_id),
            treasury,
            relayer,
            relayer_float_cost_multiplier: self.relayer_float_cost_multiplier,
            relayer_float_target_wei: self.relayer_float_target_wei,
            relayer_float_min_wei: self.relayer_float_min_wei,
            treasury_floor_wei: self.treasury_floor_wei,
        })
    }
}

impl CfConfig {
    pub fn from_env(env: &Env) -> Result<Self, String> {
        let relayer_count = match var(env, "VELA_RELAY_RELAYER_COUNT") {
            Some(value) => value
                .parse::<usize>()
                .map_err(|error| format!("invalid VELA_RELAY_RELAYER_COUNT: {error}"))?,
            None => vault::RELAYER_ROUTING_WIDTH,
        };
        if relayer_count == 0 || relayer_count > vault::RELAYER_POOL_SIZE {
            return Err(format!(
                "VELA_RELAY_RELAYER_COUNT must be 1..={}",
                vault::RELAYER_POOL_SIZE
            ));
        }

        let executor_enabled = match var(env, "VELA_RELAY_EXECUTOR_ENABLED") {
            Some(value) => parse_bool("VELA_RELAY_EXECUTOR_ENABLED", &value)?,
            None => true,
        };

        // Read per fetch by `arms::market`; checked here so a bad value fails
        // every request instead of only the uncached ones.
        chain_directory(env)?;

        let operator_secret = secret(env, "OPERATOR_SECRET");
        if executor_enabled && operator_secret.is_none() {
            return Err(
                "OPERATOR_SECRET is required when the UserOperation consumer is enabled".into(),
            );
        }

        // Same rule as the docker parser: an explicitly configured recipient
        // must match the address derived from the operator secret.
        let configured_recipient = var(env, "VELA_RELAY_SETTLEMENT_RECIPIENT");
        let settlement_recipient = match (&operator_secret, configured_recipient) {
            (Some(secret_value), configured) => {
                let derived = vault::derive_address(secret_value)
                    .map_err(|error| format!("invalid OPERATOR_SECRET: {error}"))?;
                if let Some(configured) = configured
                    && !configured.eq_ignore_ascii_case(&derived)
                {
                    return Err(
                        "VELA_RELAY_SETTLEMENT_RECIPIENT does not match the address derived from OPERATOR_SECRET"
                            .into(),
                    );
                }
                Some(derived)
            }
            (None, configured) => configured,
        };

        let execution_chains = match var(env, "VELA_RELAY_EXECUTION_CHAINS") {
            None => Vec::new(),
            Some(value) if value.trim().is_empty() => Vec::new(),
            Some(value) => value
                .split(',')
                .map(|chain| {
                    chain
                        .trim()
                        .parse::<u64>()
                        .map_err(|error| format!("invalid VELA_RELAY_EXECUTION_CHAINS: {error}"))
                })
                .collect::<Result<Vec<_>, _>>()?,
        };

        // Same pairing rule as the docker parser. The chat id is
        // account-specific, so deployments keep it in the secret store with
        // the token (a plain var also works, e.g. under `wrangler dev`).
        let telegram_bot_token = secret(env, "TELEGRAM_BOT_TOKEN").filter(|t| !t.trim().is_empty());
        let telegram_chat_id = var(env, "TELEGRAM_CHAT_ID")
            .or_else(|| secret(env, "TELEGRAM_CHAT_ID"))
            .filter(|c| !c.trim().is_empty());
        if telegram_bot_token.is_some() != telegram_chat_id.is_some() {
            return Err(
                "TELEGRAM_BOT_TOKEN and TELEGRAM_CHAT_ID must be configured together".into(),
            );
        }

        // Same defaults and bounds as the docker parser.
        let settlement_markup_bps =
            u64_var(env, "VELA_RELAY_EXECUTOR_SETTLEMENT_MARKUP_BPS", 14_000)?;
        if settlement_markup_bps < 10_000 {
            return Err("VELA_RELAY_EXECUTOR_SETTLEMENT_MARKUP_BPS cannot be below 10000".into());
        }
        let settlement_inclusion_floor_bps = u64_var(
            env,
            "VELA_RELAY_EXECUTOR_SETTLEMENT_INCLUSION_FLOOR_BPS",
            15_000,
        )?;
        if settlement_inclusion_floor_bps <= 10_000 {
            return Err(
                "VELA_RELAY_EXECUTOR_SETTLEMENT_INCLUSION_FLOOR_BPS must be above 10000".into(),
            );
        }
        let relayer_float_min_wei = u128_var(
            env,
            "VELA_RELAY_EXECUTOR_FLOAT_MIN_WEI",
            500_000_000_000_000,
        )?;
        let relayer_float_target_wei = u128_var(
            env,
            "VELA_RELAY_EXECUTOR_FLOAT_TARGET_WEI",
            2_000_000_000_000_000,
        )?;
        if relayer_float_target_wei < relayer_float_min_wei {
            return Err(
                "VELA_RELAY_EXECUTOR_FLOAT_TARGET_WEI cannot be below VELA_RELAY_EXECUTOR_FLOAT_MIN_WEI"
                    .into(),
            );
        }

        Ok(Self {
            settlement_recipient,
            relayer_count,
            executor_enabled,
            execution_chains,
            alchemy_api_key: secret(env, "ALCHEMY_API_KEY").filter(|key| !key.trim().is_empty()),
            operator_secret,
            trusted_rpc_urls: match var(env, "VELA_RELAY_EXECUTOR_RPC_URLS") {
                Some(value) => parse_trusted_rpc_urls(&value)?,
                None => std::collections::BTreeMap::new(),
            },
            rpc_timeout_ms: u64_var(env, "VELA_RELAY_EXECUTOR_RPC_TIMEOUT_SECS", 5)?
                .saturating_mul(1_000),
            lease_ttl_ms: u64_var(env, "VELA_RELAY_EXECUTOR_LEASE_TTL_SECS", 30)?
                .saturating_mul(1_000),
            receipt_poll_ms: u64_var(env, "VELA_RELAY_EXECUTOR_RECEIPT_POLL_SECS", 3)?
                .saturating_mul(1_000),
            attempt_ttl_ms: u64_var(env, "VELA_RELAY_EXECUTOR_ATTEMPT_TTL_SECS", 48 * 60 * 60)?
                .saturating_mul(1_000),
            telegram_bot_token,
            telegram_chat_id,
            telegram_cooldown_ms: u64_var(env, "VELA_RELAY_TELEGRAM_ALERT_COOLDOWN_SECS", 30 * 60)?
                .saturating_mul(1_000),
            max_bundle_operations: usize_var(env, "VELA_RELAY_MAX_BUNDLE_OPERATIONS", 10)?,
            gas_buffer_bps: u64_var(env, "VELA_RELAY_EXECUTOR_GAS_BUFFER_BPS", 1_500)?,
            fixed_gas_buffer: u64_var(env, "VELA_RELAY_EXECUTOR_FIXED_GAS_BUFFER", 30_000)?,
            settlement_markup_bps,
            settlement_inclusion_floor_bps,
            settlement_hold_max_attempts: u32_var(
                env,
                "VELA_RELAY_EXECUTOR_SETTLEMENT_HOLD_MAX_ATTEMPTS",
                12,
            )?,
            relayer_float_cost_multiplier: u64_var(
                env,
                "VELA_RELAY_EXECUTOR_FLOAT_COST_MULTIPLIER",
                5,
            )?,
            relayer_float_target_wei,
            relayer_float_min_wei,
            top_up_max_wei: u128_var(
                env,
                "VELA_RELAY_EXECUTOR_TOP_UP_MAX_WEI",
                10_000_000_000_000_000_000,
            )?,
            treasury_floor_wei: u128_var(
                env,
                "VELA_RELAY_EXECUTOR_TREASURY_FLOOR_WEI",
                100_000_000_000_000,
            )?,
        })
    }
}

/// The docker parser's `parse_trusted_rpc_urls`, error strings included:
/// a JSON map of chain ID → URL or URL array, http/https only.
fn parse_trusted_rpc_urls(
    value: &str,
) -> Result<std::collections::BTreeMap<u64, Vec<String>>, String> {
    let values =
        serde_json::from_str::<std::collections::BTreeMap<String, serde_json::Value>>(value)
            .map_err(|error| format!("invalid VELA_RELAY_EXECUTOR_RPC_URLS: {error}"))?;
    let mut result = std::collections::BTreeMap::new();

    for (chain_id, urls) in values {
        let chain_id = chain_id.parse::<u64>().map_err(|error| {
            format!("invalid chain ID in VELA_RELAY_EXECUTOR_RPC_URLS: {error}")
        })?;
        let urls = match urls {
            serde_json::Value::String(url) => vec![url],
            serde_json::Value::Array(urls) => urls
                .into_iter()
                .map(|url| {
                    url.as_str().map(str::to_owned).ok_or_else(|| {
                        "VELA_RELAY_EXECUTOR_RPC_URLS values must be URLs or URL arrays".to_owned()
                    })
                })
                .collect::<Result<Vec<_>, _>>()?,
            _ => {
                return Err(
                    "VELA_RELAY_EXECUTOR_RPC_URLS values must be URLs or URL arrays".into(),
                );
            }
        };
        if urls.is_empty() {
            return Err(format!(
                "VELA_RELAY_EXECUTOR_RPC_URLS chain {chain_id} has no URL"
            ));
        }
        for url in &urls {
            let parsed = worker::Url::parse(url).map_err(|error| {
                format!(
                    "invalid RPC URL for chain {chain_id} in VELA_RELAY_EXECUTOR_RPC_URLS: {error}"
                )
            })?;
            if !matches!(parsed.scheme(), "http" | "https") {
                return Err(format!(
                    "RPC URL for chain {chain_id} must use http or https"
                ));
            }
        }
        result.insert(chain_id, urls);
    }

    Ok(result)
}

fn u64_var(env: &Env, name: &str, default: u64) -> Result<u64, String> {
    match var(env, name) {
        None => Ok(default),
        Some(value) => value
            .parse::<u64>()
            .map_err(|error| format!("invalid {name}: {error}")),
    }
}

fn u32_var(env: &Env, name: &str, default: u32) -> Result<u32, String> {
    match var(env, name) {
        None => Ok(default),
        Some(value) => value
            .parse::<u32>()
            .map_err(|error| format!("invalid {name}: {error}")),
    }
}

fn u128_var(env: &Env, name: &str, default: u128) -> Result<u128, String> {
    match var(env, name) {
        None => Ok(default),
        Some(value) => value
            .parse::<u128>()
            .map_err(|error| format!("invalid {name}: {error}")),
    }
}

fn usize_var(env: &Env, name: &str, default: usize) -> Result<usize, String> {
    match var(env, name) {
        None => Ok(default),
        Some(value) => value
            .parse::<usize>()
            .map_err(|error| format!("invalid {name}: {error}")),
    }
}

/// Same setting and rule as the docker shell; absent = Vela's directory.
pub fn chain_directory(env: &Env) -> Result<ChainDirectory, String> {
    ChainDirectory::from_setting(var(env, "VELA_RELAY_CHAIN_DIRECTORY_URL").as_deref())
        .map_err(|error| format!("invalid VELA_RELAY_CHAIN_DIRECTORY_URL: {error}"))
}

fn var(env: &Env, name: &str) -> Option<String> {
    env.var(name).ok().map(|value| value.to_string())
}

fn secret(env: &Env, name: &str) -> Option<String> {
    env.secret(name).ok().map(|value| value.to_string())
}

fn parse_bool(name: &str, value: &str) -> Result<bool, String> {
    // Same vocabulary as the docker parser.
    match value.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        _ => Err(format!("invalid {name}; expected true or false")),
    }
}
