//! `config.json` + `.env` schemas (SPEC §2 tree roots, §11.4 secrets).
//!
//! Typed schema for orchestrator/engine settings (`config.json`) and the
//! `.env` secret surface (mode 0600, dotenvy-hydrated out-of-band). Secrets
//! never serialize into charter.json, archives, logs, or test fixtures
//! (CONVENTIONS §6); schema types make leakage unrepresentable:
//! [`Secret`] has no [`serde::Serialize`] impl and a redacting `Debug`, so
//! [`Secrets`] cannot be marshalled into any hamlet artifact by accident.
//!
//! Everything here is pure (ADR-0001): parsing/rendering operate on strings
//! and injected readers; file I/O belongs to the caller (`hamlet init`,
//! daemon).
//!
//! # `config.json` versioning
//!
//! `config_version` is mandatory; migrations are additive-only with explicit
//! version steps (CONVENTIONS §3). Unknown fields are rejected at parse time
//! so operator typos fail loudly instead of silently dropping settings.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

/// Schema version this build writes and accepts (CONVENTIONS §3).
///
/// v1 → v2 (Sprint 02): additive `resources` section (container resource
/// limit placeholders). v1 documents still parse — the new field carries
/// `#[serde(default)]`, so migration is "parse old, write new"; no rewrite
/// of existing trees is required.
pub const CONFIG_VERSION: u32 = 2;

/// Default daemon data dir (sled telemetry location). Relative paths
/// resolve against the *working directory containing* the village root —
/// see [`Config::resolve_daemon_data_dir`] — so the default lands at
/// `<working-dir>/.hamlet-data/sled`, a sibling of `.hamlet/`, never inside
/// the tree: telemetry is out-of-band (ADR-0005).
const DEFAULT_DAEMON_DATA_DIR: &str = ".hamlet-data/sled";
/// Default podman binary; resolved via `PATH` by the daemon (SPEC §4:
/// rootless podman CLI only).
const DEFAULT_PODMAN_BIN: &str = "podman";
/// Default base image (SPEC §4: slim Alpine + uutils). Sprint 02 builds the
/// real hamlet base image; until then the vanilla reference is the default
/// and operators may override it in `config.json`.
const DEFAULT_BASE_IMAGE: &str = "docker.io/library/alpine:3.20";
/// Default web listen address (SPEC §12.3): loopback-only until an operator
/// explicitly widens it.
const DEFAULT_WEB_LISTEN_ADDR: &str = "127.0.0.1:8787";
/// Default per-request webhook dispatch timeout (SPEC §8.4).
const DEFAULT_DISPATCH_TIMEOUT_SECS: u64 = 30;
/// Default container memory limit (conservative until the Sprint 07 policy
/// ADR fixes the real curve).
const DEFAULT_CONTAINER_MEMORY: &str = "512m";
/// Default container CPU limit (`--cpus` value).
const DEFAULT_CONTAINER_CPUS: &str = "1.0";
/// Default container PID limit (`--pids-limit`); fork-bomb brace.
const DEFAULT_CONTAINER_PIDS_LIMIT: u64 = 256;

/// Orchestrator/engine settings — the typed form of `.hamlet/config.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Schema version; must equal [`CONFIG_VERSION`] (migrations are
    /// additive-only with explicit version steps).
    pub config_version: u32,
    /// Daemon data dir (sled telemetry location, SPEC §11.2). Relative
    /// paths resolve against the working directory containing the village
    /// root, never inside the `.hamlet/` tree (ADR-0005); telemetry is
    /// never orchestration state (CONVENTIONS §2).
    pub daemon_data_dir: PathBuf,
    /// Podman binary path (rootless CLI only, SPEC §4). `PATH`-resolved when
    /// relative.
    pub podman_bin: PathBuf,
    /// Base image reference for agent containers (SPEC §4).
    pub base_image: String,
    /// Web/WS listen address (`socketaddr` form, SPEC §12.3).
    pub web_listen_addr: String,
    /// Daemon log level.
    pub log_level: LogLevel,
    /// External webhook dispatch settings (placeholder — SPEC §8.4 policy is
    /// [DEV]; endpoint auth lives in `.env`, never here).
    pub dispatch: DispatchConfig,
    /// Container resource-limit placeholders (SPEC §4; the binding policy
    /// ADR is Sprint 07 — values here are wired into every `podman run` as
    /// `--memory` / `--cpus` / `--pids-limit`). Absent in v1 documents;
    /// defaults apply.
    #[serde(default)]
    pub resources: ResourcesConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            config_version: CONFIG_VERSION,
            daemon_data_dir: PathBuf::from(DEFAULT_DAEMON_DATA_DIR),
            podman_bin: PathBuf::from(DEFAULT_PODMAN_BIN),
            base_image: DEFAULT_BASE_IMAGE.to_string(),
            web_listen_addr: DEFAULT_WEB_LISTEN_ADDR.to_string(),
            log_level: LogLevel::default(),
            dispatch: DispatchConfig::default(),
            resources: ResourcesConfig::default(),
        }
    }
}

impl Config {
    /// Parse + validate `config.json` contents.
    ///
    /// # Errors
    ///
    /// [`ConfigError::Syntax`] on malformed JSON or unknown fields, then
    /// whatever [`Config::validate`] reports.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let config: Self =
            serde_json::from_str(text).map_err(|e| ConfigError::Syntax(e.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    /// Canonical human-readable rendering (pretty JSON, trailing newline) —
    /// what `hamlet init` writes to `config.json`.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = serde_json::to_string_pretty(self)
            .expect("plain-struct Config serialization is infallible");
        out.push('\n');
        out
    }

    /// Validate every field. Called by [`Config::parse`]; exposed for daemon
    /// re-validation after edits.
    ///
    /// # Errors
    ///
    /// [`ConfigError::UnsupportedVersion`] or [`ConfigError::Invalid`] with a
    /// field-qualified message.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.config_version == 0 || self.config_version > CONFIG_VERSION {
            return Err(ConfigError::UnsupportedVersion(self.config_version));
        }
        if self.daemon_data_dir.as_os_str().is_empty() {
            return Err(ConfigError::Invalid(
                "daemon_data_dir must not be empty".to_string(),
            ));
        }
        if self.podman_bin.as_os_str().is_empty() {
            return Err(ConfigError::Invalid(
                "podman_bin must not be empty".to_string(),
            ));
        }
        if self.base_image.is_empty() {
            return Err(ConfigError::Invalid(
                "base_image must not be empty".to_string(),
            ));
        }
        if self
            .base_image
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(ConfigError::Invalid(
                "base_image must not contain whitespace or control characters".to_string(),
            ));
        }
        if self
            .web_listen_addr
            .parse::<std::net::SocketAddr>()
            .is_err()
        {
            return Err(ConfigError::Invalid(format!(
                "web_listen_addr must be a socket address (host:port), got {:?}",
                self.web_listen_addr
            )));
        }
        self.dispatch.validate()?;
        self.resources.validate()
    }

    /// Resolve [`Config::daemon_data_dir`] for the village rooted at
    /// `village_root` (the `.hamlet/` dir): absolute paths pass through;
    /// relative paths resolve against the *working directory containing*
    /// the village root, so telemetry never lands inside the tree
    /// (out-of-band per SPEC §11.2; ratified by ADR-0005). Pure — the
    /// caller performs any I/O.
    #[must_use]
    pub fn resolve_daemon_data_dir(&self, village_root: &Path) -> PathBuf {
        if self.daemon_data_dir.is_relative() {
            village_root
                .parent()
                .unwrap_or(village_root)
                .join(&self.daemon_data_dir)
        } else {
            self.daemon_data_dir.clone()
        }
    }
}

/// Daemon log level (`config.json` field `log_level`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Error,
    Warn,
    #[default]
    Info,
    Debug,
    Trace,
}

impl fmt::Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        };
        f.write_str(name)
    }
}

/// Webhook dispatch settings (SPEC §8.4). Placeholder: retry/backoff policy
/// and the outbox/inbox file schema land with the courier work; endpoint
/// auth comes from `.env` secrets, never from `config.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DispatchConfig {
    /// Whether the daemon POSTs `courier/dispatch/outbox/` files at all.
    pub enabled: bool,
    /// Per-request HTTP timeout, seconds (1..=3600).
    pub timeout_secs: u64,
}

impl Default for DispatchConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            timeout_secs: DEFAULT_DISPATCH_TIMEOUT_SECS,
        }
    }
}

impl DispatchConfig {
    /// # Errors
    ///
    /// [`ConfigError::Invalid`] when `timeout_secs` is outside 1..=3600.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.timeout_secs == 0 || self.timeout_secs > 3600 {
            return Err(ConfigError::Invalid(format!(
                "dispatch.timeout_secs must be in 1..=3600, got {}",
                self.timeout_secs
            )));
        }
        Ok(())
    }
}

fn memory_re() -> &'static Regex {
    // podman --memory byte-unit form: digits + optional b/k/m/g suffix.
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[0-9]+[bkmgBKMG]?$").unwrap())
}

fn cpus_re() -> &'static Regex {
    // podman --cpus: decimal or percentage form.
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[0-9]+(\.[0-9]+)?%?$").unwrap())
}

/// Container resource-limit settings (SPEC §4 hardening; policy **[DEV]**
/// until the Sprint 07 ADR — these are the `config.json` placeholders the
/// daemon's podman layer wires into every `podman run`). Defaults are
/// deliberately conservative; values are charset-validated because they
/// land verbatim in argv (ADR-0010: typed sources only, never a shell).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourcesConfig {
    /// `--memory` value, e.g. `"512m"` (digits + optional b/k/m/g suffix).
    pub memory: String,
    /// `--cpus` value, e.g. `"1.0"` or `"50%"`.
    pub cpus: String,
    /// `--pids-limit` (1..=65535; unlimited is never a default).
    pub pids_limit: u64,
}

impl Default for ResourcesConfig {
    fn default() -> Self {
        Self {
            memory: DEFAULT_CONTAINER_MEMORY.to_string(),
            cpus: DEFAULT_CONTAINER_CPUS.to_string(),
            pids_limit: DEFAULT_CONTAINER_PIDS_LIMIT,
        }
    }
}

impl ResourcesConfig {
    /// # Errors
    ///
    /// [`ConfigError::Invalid`] when a value falls outside the charset or
    /// range podman (and ADR-0010 argv safety) accepts.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !memory_re().is_match(&self.memory) {
            return Err(ConfigError::Invalid(format!(
                "resources.memory must match <digits>[bkmg], got {:?}",
                self.memory
            )));
        }
        if !cpus_re().is_match(&self.cpus) {
            return Err(ConfigError::Invalid(format!(
                "resources.cpus must be a decimal or percentage, got {:?}",
                self.cpus
            )));
        }
        if self.pids_limit == 0 || self.pids_limit > 65535 {
            return Err(ConfigError::Invalid(format!(
                "resources.pids_limit must be in 1..=65535, got {}",
                self.pids_limit
            )));
        }
        Ok(())
    }
}

/// `config.json` parse/validation failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// Malformed JSON, wrong field types, or unknown fields.
    Syntax(String),
    /// `config_version` is 0 or newer than this build supports.
    UnsupportedVersion(u32),
    /// A field failed validation (message names the field).
    Invalid(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Syntax(msg) => write!(f, "config.json syntax error: {msg}"),
            Self::UnsupportedVersion(v) => write!(
                f,
                "unsupported config_version {v} (this build writes/accepts {CONFIG_VERSION})"
            ),
            Self::Invalid(msg) => write!(f, "invalid config: {msg}"),
        }
    }
}

impl std::error::Error for ConfigError {}

// --- .env secrets (SPEC §11.4; CONVENTIONS §6) -----------------------------

/// `.env` variable: model endpoint used by agent sessions.
pub const ENV_MODEL_ENDPOINT: &str = "HAMLET_MODEL_ENDPOINT";
/// `.env` variable: default model API key.
pub const ENV_MODEL_API_KEY: &str = "HAMLET_MODEL_API_KEY";
/// `.env` prefix: additional provider keys (`HAMLET_API_KEY_<PROVIDER>`).
pub const ENV_API_KEY_PREFIX: &str = "HAMLET_API_KEY_";

/// The `.env` template written by `hamlet init` (mode 0600). Every entry is
/// commented out and every key value is an obvious fake
/// (`test-key-do-not-use`) so a freshly initialized tree contains no
/// material that could ever be mistaken for a real secret.
pub const ENV_TEMPLATE: &str = "\
# hamlet .env — secrets (SPEC §11.4; CONVENTIONS §6).
# Mode 0600. Never commit this file. Hydrated via dotenvy into the daemon
# and injected into agent sessions out-of-band; values here are NEVER
# written to charter.json, archives, or logs.
#
# Uncomment and replace the placeholders below. All sample values are
# obvious fakes.

# Model endpoint used by agent sessions:
#HAMLET_MODEL_ENDPOINT=https://model.example.invalid/v1

# Default model API key:
#HAMLET_MODEL_API_KEY=test-key-do-not-use

# Additional provider keys (HAMLET_API_KEY_<PROVIDER>):
#HAMLET_API_KEY_ANTHROPIC=test-key-do-not-use
#HAMLET_API_KEY_OPENAI=test-key-do-not-use
";

/// A single secret value. Deliberately has **no** [`serde::Serialize`] impl
/// and no [`fmt::Display`]: writing a secret into charter.json, logs, or
/// archives does not type-check. The only read path is [`Secret::expose`],
/// which is greppable at review time. `Debug` redacts.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// Wrap a secret value.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The only accessor to the raw value (out-of-band injection points
    /// only — never logging, never charter serialization).
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

/// Typed secret surface of `.env` (SPEC §11.4). Only the variables hamlet
/// understands are collected; the rest of the environment is never mirrored.
/// No `Serialize` derive — see [`Secret`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Secrets {
    /// `HAMLET_MODEL_ENDPOINT`
    pub model_endpoint: Option<Secret>,
    /// `HAMLET_MODEL_API_KEY`
    pub model_api_key: Option<Secret>,
    /// `HAMLET_API_KEY_<PROVIDER>` → provider name (suffix as written).
    /// `BTreeMap`: sorted iteration, always (CONVENTIONS §3).
    pub provider_keys: BTreeMap<String, Secret>,
}

impl Secrets {
    /// Collect secrets from already-parsed `(key, value)` pairs. Empty
    /// values are treated as unset. Unknown variables are ignored.
    #[must_use]
    pub fn from_vars<K, V, I>(vars: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let mut secrets = Self::default();
        for (key, value) in vars {
            let (key, value) = (key.as_ref(), value.as_ref());
            if value.is_empty() {
                continue;
            }
            if key == ENV_MODEL_ENDPOINT {
                secrets.model_endpoint = Some(Secret::new(value));
            } else if key == ENV_MODEL_API_KEY {
                secrets.model_api_key = Some(Secret::new(value));
            } else if let Some(provider) = key.strip_prefix(ENV_API_KEY_PREFIX) {
                if !provider.is_empty() {
                    secrets
                        .provider_keys
                        .insert(provider.to_string(), Secret::new(value));
                }
            }
        }
        secrets
    }

    /// Parse `.env` contents from an injected reader via dotenvy's pure
    /// iterator parser (`from_read_iter` — never `from_read`, which would
    /// mutate the process environment and breach the ADR-0001 purity seam),
    /// then collect via [`Secrets::from_vars`]. The caller opens the file.
    ///
    /// # Errors
    ///
    /// [`SecretsError::Parse`] on malformed `.env` syntax or read failure.
    pub fn from_read(reader: impl std::io::Read) -> Result<Self, SecretsError> {
        // BTreeMap first: duplicate keys resolve to the last *non-empty*
        // value in sorted, deterministic order regardless of file ordering
        // (an empty value means unset and must not shadow a real one).
        let mut vars = BTreeMap::new();
        for item in dotenvy::from_read_iter(reader) {
            let (key, value) = item.map_err(|e| SecretsError::Parse(e.to_string()))?;
            if !value.is_empty() {
                vars.insert(key, value);
            }
        }
        Ok(Self::from_vars(
            vars.iter().map(|(k, v)| (k.as_str(), v.as_str())),
        ))
    }

    /// True when no secret material was collected.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.model_endpoint.is_none()
            && self.model_api_key.is_none()
            && self.provider_keys.is_empty()
    }
}

/// `.env` parse failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretsError {
    /// Malformed `.env` syntax or underlying read error (message only — the
    /// error never carries secret values).
    Parse(String),
}

impl fmt::Display for SecretsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(msg) => write!(f, ".env parse error: {msg}"),
        }
    }
}

impl std::error::Error for SecretsError {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Committed golden vector: the exact bytes `hamlet init` writes to
    /// `config.json` (defaults, `config_version` 1).
    #[test]
    fn default_config_render_golden() {
        let expected = "\
{
  \"config_version\": 2,
  \"daemon_data_dir\": \".hamlet-data/sled\",
  \"podman_bin\": \"podman\",
  \"base_image\": \"docker.io/library/alpine:3.20\",
  \"web_listen_addr\": \"127.0.0.1:8787\",
  \"log_level\": \"info\",
  \"dispatch\": {
    \"enabled\": false,
    \"timeout_secs\": 30
  },
  \"resources\": {
    \"memory\": \"512m\",
    \"cpus\": \"1.0\",
    \"pids_limit\": 256
  }
}
";
        assert_eq!(Config::default().render(), expected);
    }

    /// v1 → v2 migration is additive: a v1 document (no `resources` block)
    /// still parses, defaults fill in, and the declared version is left
    /// untouched (CONVENTIONS §3.3 — no silent rewrites).
    #[test]
    fn v1_config_parses_with_default_resources() {
        let v1 = "\
{
  \"config_version\": 1,
  \"daemon_data_dir\": \".hamlet-data/sled\",
  \"podman_bin\": \"podman\",
  \"base_image\": \"docker.io/library/alpine:3.20\",
  \"web_listen_addr\": \"127.0.0.1:8787\",
  \"log_level\": \"info\",
  \"dispatch\": {
    \"enabled\": false,
    \"timeout_secs\": 30
  }
}
";
        let parsed = Config::parse(v1).expect("v1 must still parse");
        assert_eq!(parsed.config_version, 1);
        assert_eq!(parsed.resources, ResourcesConfig::default());
    }

    #[test]
    fn resources_validation_rejects_bad_values() {
        for bad in ["", "x", "-512m", "512 m", "512mb", "1;rm"] {
            let cfg = Config {
                resources: ResourcesConfig {
                    memory: bad.to_string(),
                    ..Default::default()
                },
                ..Default::default()
            };
            assert!(
                matches!(cfg.validate(), Err(ConfigError::Invalid(_))),
                "memory {bad:?} must be rejected"
            );
        }
        for bad in ["", "one", "1.0.0", "--cpus", "50pct"] {
            let cfg = Config {
                resources: ResourcesConfig {
                    cpus: bad.to_string(),
                    ..Default::default()
                },
                ..Default::default()
            };
            assert!(
                matches!(cfg.validate(), Err(ConfigError::Invalid(_))),
                "cpus {bad:?} must be rejected"
            );
        }
        for bad in [0u64, 65536, u64::MAX] {
            let cfg = Config {
                resources: ResourcesConfig {
                    pids_limit: bad,
                    ..Default::default()
                },
                ..Default::default()
            };
            assert!(
                matches!(cfg.validate(), Err(ConfigError::Invalid(_))),
                "pids_limit {bad} must be rejected"
            );
        }
        for memory in ["512m", "2g", "1024"] {
            for cpus in ["1", "1.0", "50%"] {
                let cfg = Config {
                    resources: ResourcesConfig {
                        memory: memory.to_string(),
                        cpus: cpus.to_string(),
                        pids_limit: 256,
                    },
                    ..Default::default()
                };
                assert!(cfg.validate().is_ok(), "{memory}/{cpus} must pass");
            }
        }
    }

    #[test]
    fn default_config_roundtrips_through_parse() {
        let rendered = Config::default().render();
        assert_eq!(Config::parse(&rendered).unwrap(), Config::default());
    }

    #[test]
    fn parse_rejects_bad_version() {
        for version in [0, CONFIG_VERSION + 1, u32::MAX] {
            let cfg = Config {
                config_version: version,
                ..Default::default()
            };
            assert_eq!(
                Config::parse(&cfg.render()),
                Err(ConfigError::UnsupportedVersion(version))
            );
        }
    }

    #[test]
    fn parse_rejects_unknown_fields() {
        let text = Config::default().render().replace(
            "\"log_level\": \"info\",",
            "\"log_level\": \"info\",\n  \"surprise\": true,",
        );
        assert!(matches!(Config::parse(&text), Err(ConfigError::Syntax(_))));
    }

    #[test]
    fn validate_rejects_bad_fields() {
        let bad = [
            Config {
                daemon_data_dir: PathBuf::new(),
                ..Default::default()
            },
            Config {
                podman_bin: PathBuf::new(),
                ..Default::default()
            },
            Config {
                base_image: "alpine 3".to_string(),
                ..Default::default()
            },
            Config {
                web_listen_addr: "not-an-addr".to_string(),
                ..Default::default()
            },
            Config {
                dispatch: DispatchConfig {
                    enabled: false,
                    timeout_secs: 0,
                },
                ..Default::default()
            },
            Config {
                dispatch: DispatchConfig {
                    enabled: false,
                    timeout_secs: 3601,
                },
                ..Default::default()
            },
        ];
        for cfg in bad {
            assert!(
                matches!(cfg.validate(), Err(ConfigError::Invalid(_))),
                "must reject: {cfg:?}"
            );
        }
    }

    #[test]
    fn validate_accepts_socket_addr_forms() {
        for addr in ["127.0.0.1:8787", "[::1]:9000", "0.0.0.0:1"] {
            let cfg = Config {
                web_listen_addr: addr.to_string(),
                ..Default::default()
            };
            assert_eq!(cfg.validate(), Ok(()), "addr {addr} must be accepted");
        }
    }

    #[test]
    fn resolve_daemon_data_dir() {
        let cfg = Config::default();
        // Default resolves OUTSIDE the village tree (ADR-0005): sibling
        // `.hamlet-data/` in the working directory, never inside `.hamlet/`.
        let resolved = cfg.resolve_daemon_data_dir(Path::new("/w/.hamlet"));
        assert_eq!(resolved, PathBuf::from("/w/.hamlet-data/sled"));
        assert!(!resolved.starts_with("/w/.hamlet"));
        let cfg = Config {
            daemon_data_dir: PathBuf::from("/var/lib/hamlet"),
            ..Default::default()
        };
        assert_eq!(
            cfg.resolve_daemon_data_dir(Path::new("/w/.hamlet")),
            PathBuf::from("/var/lib/hamlet")
        );
    }

    /// The committed init template must parse (via dotenvy) to *empty*
    /// secrets: every entry is commented out, every value an obvious fake.
    #[test]
    fn env_template_parses_to_empty_secrets() {
        let secrets = Secrets::from_read(ENV_TEMPLATE.as_bytes()).unwrap();
        assert!(secrets.is_empty());
        assert_eq!(secrets, Secrets::default());
    }

    #[test]
    fn env_template_holds_no_real_looking_values() {
        for line in ENV_TEMPLATE
            .lines()
            .filter(|l| !l.trim_start().starts_with('#'))
        {
            assert!(
                line.trim().is_empty(),
                "template must be all comments: {line:?}"
            );
        }
        assert!(ENV_TEMPLATE.contains("test-key-do-not-use"));
    }

    #[test]
    fn secrets_from_read_collects_typed_vars() {
        let env = "\
HAMLET_MODEL_ENDPOINT=https://model.example.invalid/v1
HAMLET_MODEL_API_KEY=test-key-do-not-use
HAMLET_API_KEY_OPENAI=test-key-do-not-use
HAMLET_API_KEY_ANTHROPIC=test-key-do-not-use-2
UNRELATED=nope
HAMLET_API_KEY_=dangling-prefix-ignored
HAMLET_MODEL_API_KEY=
";
        let secrets = Secrets::from_read(env.as_bytes()).unwrap();
        assert_eq!(
            secrets.model_endpoint.as_ref().unwrap().expose(),
            "https://model.example.invalid/v1"
        );
        assert_eq!(
            secrets.model_api_key.as_ref().unwrap().expose(),
            "test-key-do-not-use"
        );
        let providers: Vec<(&str, &str)> = secrets
            .provider_keys
            .iter()
            .map(|(k, v)| (k.as_str(), v.expose()))
            .collect();
        assert_eq!(
            providers,
            vec![
                ("ANTHROPIC", "test-key-do-not-use-2"),
                ("OPENAI", "test-key-do-not-use")
            ]
        );
        assert!(!secrets.is_empty());
    }

    /// CONVENTIONS §6: secrets must never reach logs. The `Debug` impls
    /// redact every value; there is no `Serialize` impl to misuse.
    #[test]
    fn secrets_debug_redacts_values() {
        let secrets = Secrets::from_vars(vec![
            ("HAMLET_MODEL_ENDPOINT", "https://model.example.invalid/v1"),
            ("HAMLET_MODEL_API_KEY", "test-key-do-not-use"),
            ("HAMLET_API_KEY_OPENAI", "test-key-do-not-use-2"),
        ]);
        let dumped = format!("{secrets:?}");
        assert!(!dumped.contains("test-key-do-not-use"), "{dumped}");
        assert!(!dumped.contains("model.example.invalid"), "{dumped}");
        assert!(dumped.contains("***"), "{dumped}");
        // Provider *names* are not secret and stay visible for diagnostics.
        assert!(dumped.contains("OPENAI"), "{dumped}");
    }

    #[test]
    fn secrets_from_read_rejects_malformed_env() {
        // Unbalanced quote → dotenvy syntax error, message carries no value.
        let err = Secrets::from_read("HAMLET_MODEL_API_KEY=\"unterminated\n".as_bytes())
            .expect_err("malformed .env must fail");
        assert!(matches!(err, SecretsError::Parse(_)));
    }
}
