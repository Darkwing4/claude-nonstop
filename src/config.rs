use std::collections::BTreeMap;
use std::env;
use std::path::Path;

use serde::{Deserialize, Serialize};
use toml_edit::{DocumentMut, Item, Table, Value};

use crate::atomic_file;
use crate::error::{Error, Result};
use crate::paths::Paths;

const ENV_PREFIX: &str = "CLAUDE_NONSTOP_";
const NON_CONFIG_ENV: &[&str] = &[
    "ACTIVE",
    "CONFIG",
    "STATE_DIR",
    "INSTALL_DIR",
    "VERSION",
    "DEBUG",
];
const RETIRED_KEYS: &[(&str, &str)] = &[("notify", "channel")];

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub general: General,
    pub agents: Agents,
    pub cutoff: Cutoff,
    pub overload: Overload,
    pub limit: Limit,
    pub notify: Notify,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct General {
    pub auto_resume: bool,
    pub menu_auto_answer: bool,
    pub multiplexer: MultiplexerChoice,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MultiplexerChoice {
    Auto,
    Tmux,
    Zellij,
    Headless,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Agents {
    pub claude: AgentConfig,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentConfig {
    pub enabled: bool,
    pub launch_args: Vec<String>,
    pub resume_args: Vec<String>,
    pub resume_message: String,
    pub cutoff_message: String,
    pub overload_message: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Cutoff {
    pub max_in_a_row: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Overload {
    pub message_contains: Vec<String>,
    pub backoff_seconds: Vec<u32>,
    pub steady_seconds: u32,
    pub jitter_percent: u32,
    pub max_total_minutes: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limit {
    pub margin_seconds: u32,
    pub fallback_hours: u32,
    pub probe_minutes: Vec<u32>,
    pub max_attempts: u32,
    pub verify_seconds: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Notify {
    pub enabled: bool,
    pub ntfy_topic: String,
    pub ntfy_server: String,
    pub ntfy_token: String,
    pub ntfy_privacy: NtfyPrivacy,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NtfyPrivacy {
    Full,
    Terse,
}

pub const RESUME_MESSAGE: &str = "Continue where you left off. The session was paused for a while and can go on now. Pick up the task you were working on and finish it.";
pub const CUTOFF_MESSAGE: &str = "Your response above was cut off mid-stream. Resume directly from where it stops, no apology, no recap. If none of it survived, answer the request from the start.";
pub const OVERLOAD_MESSAGE: &str =
    "Continue where you left off; the previous request did not go through.";
const DETECTOR_WORDS: &[&str] = &[
    "limit",
    "retry",
    "retrying",
    "attempt",
    "interrupt",
    "api error",
    "overloaded",
    "rate limited",
];

impl Default for General {
    fn default() -> Self {
        General {
            auto_resume: true,
            menu_auto_answer: true,
            multiplexer: MultiplexerChoice::Auto,
        }
    }
}

impl Default for AgentConfig {
    fn default() -> Self {
        AgentConfig {
            enabled: true,
            launch_args: Vec::new(),
            resume_args: Vec::new(),
            resume_message: RESUME_MESSAGE.to_owned(),
            cutoff_message: CUTOFF_MESSAGE.to_owned(),
            overload_message: OVERLOAD_MESSAGE.to_owned(),
        }
    }
}

impl Default for Cutoff {
    fn default() -> Self {
        Cutoff { max_in_a_row: 3 }
    }
}

impl Default for Overload {
    fn default() -> Self {
        Overload {
            message_contains: Vec::new(),
            backoff_seconds: vec![30, 60, 120, 240, 300],
            steady_seconds: 300,
            jitter_percent: 15,
            max_total_minutes: 120,
        }
    }
}

impl Overload {
    pub fn matches_message(&self, message: &str) -> bool {
        let lowered = message.to_ascii_lowercase();
        [
            "server is temporarily limiting requests (not your usage limit)",
            "only reserved subscriptions are left and their shared part is used up",
        ]
        .iter()
        .any(|pattern| lowered.contains(pattern))
            || self
                .message_contains
                .iter()
                .any(|pattern| lowered.contains(&pattern.to_ascii_lowercase()))
    }
}

impl Default for Limit {
    fn default() -> Self {
        Limit {
            margin_seconds: 60,
            fallback_hours: 5,
            probe_minutes: vec![15, 30, 60],
            max_attempts: 5,
            verify_seconds: 20,
        }
    }
}

impl Default for Notify {
    fn default() -> Self {
        Notify {
            enabled: true,
            ntfy_topic: String::new(),
            ntfy_server: "https://ntfy.sh".to_owned(),
            ntfy_token: String::new(),
            ntfy_privacy: NtfyPrivacy::Full,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueSource {
    Default,
    File,
    Env,
}

#[derive(Debug)]
pub struct Loaded {
    pub config: Config,
    pub sources: BTreeMap<String, ValueSource>,
}

pub fn load(paths: &Paths) -> Result<Loaded> {
    let env_vars: Vec<(String, String)> = env::vars()
        .filter(|(name, _)| name.starts_with(ENV_PREFIX))
        .collect();
    load_from(paths, &env_vars)
}

pub fn load_from(paths: &Paths, env_vars: &[(String, String)]) -> Result<Loaded> {
    let defaults = defaults_document()?;
    let mut merged = defaults.clone();
    let mut sources: BTreeMap<String, ValueSource> = flat_keys(defaults.as_table(), "")
        .into_iter()
        .map(|key| (key, ValueSource::Default))
        .collect();
    if let Some(mut file_doc) = read_document(&paths.config_file)? {
        remove_retired_keys(&mut file_doc);
        for key in flat_keys(file_doc.as_table(), "") {
            if !sources.contains_key(&key) {
                return Err(Error::Toml {
                    path: paths.config_file.clone(),
                    detail: format!("unknown key {key}"),
                });
            }
            let value = lookup(file_doc.as_table(), &key)
                .and_then(Item::as_value)
                .cloned();
            if let Some(value) = value {
                set_value(merged.as_table_mut(), &key, value);
                sources.insert(key, ValueSource::File);
            }
        }
    }
    for (name, raw) in env_vars {
        let suffix = &name[ENV_PREFIX.len()..];
        if NON_CONFIG_ENV.contains(&suffix) || is_retired_env(suffix) {
            continue;
        }

        let key = sources
            .keys()
            .find(|key| env_name(key) == suffix)
            .cloned()
            .ok_or_else(|| Error::Invalid(format!("{name}: unknown configuration key")))?;
        let template = lookup(defaults.as_table(), &key)
            .and_then(Item::as_value)
            .ok_or_else(|| Error::Invalid(format!("{name}: unknown configuration key")))?;
        let value = parse_env_value(raw, template)
            .map_err(|detail| Error::Invalid(format!("{name}: {detail}")))?;
        set_value(merged.as_table_mut(), &key, value);
        sources.insert(key, ValueSource::Env);
    }
    let config: Config = toml_edit::de::from_document(merged).map_err(|error| Error::Toml {
        path: paths.config_file.clone(),
        detail: error.to_string(),
    })?;
    validate(&config)?;
    Ok(Loaded { config, sources })
}

pub fn set(paths: &Paths, key: &str, raw: &str) -> Result<()> {
    let defaults = defaults_document()?;
    let template = lookup(defaults.as_table(), key)
        .and_then(Item::as_value)
        .ok_or_else(|| Error::Invalid(format!("unknown configuration key {key}")))?;
    let value = parse_env_value(raw, template)
        .map_err(|detail| Error::Invalid(format!("{key}: {detail}")))?;
    let mut doc = read_document(&paths.config_file)?.unwrap_or_default();
    remove_retired_keys(&mut doc);
    set_value(doc.as_table_mut(), key, value);
    let candidate: Config = toml_edit::de::from_document(doc.clone())
        .map_err(|error| Error::Invalid(format!("{key}: {error}")))?;
    validate(&candidate)?;
    if let Some(parent) = paths.config_file.parent() {
        crate::paths::create_private_dir(parent)?;
    }
    atomic_file::write(&paths.config_file, doc.to_string().as_bytes(), 0o600)
}

pub fn get(loaded: &Loaded, key: &str) -> Result<String> {
    let doc = document_of(&loaded.config).map_err(|error| Error::Invalid(error.to_string()))?;
    let item = lookup(doc.as_table(), key)
        .ok_or_else(|| Error::Invalid(format!("unknown configuration key {key}")))?;
    Ok(render(item))
}

pub fn list(loaded: &Loaded) -> Result<Vec<(String, String, ValueSource)>> {
    let doc = document_of(&loaded.config).map_err(|error| Error::Invalid(error.to_string()))?;
    let mut rows = Vec::new();
    for key in flat_keys(doc.as_table(), "") {
        let item = lookup(doc.as_table(), &key)
            .ok_or_else(|| Error::Invalid(format!("missing key {key}")))?;
        let source = loaded
            .sources
            .get(&key)
            .copied()
            .unwrap_or(ValueSource::Default);
        rows.push((key, render(item), source));
    }
    Ok(rows)
}

pub fn env_name(key: &str) -> String {
    key.replace('.', "_").to_ascii_uppercase()
}

fn validate(config: &Config) -> Result<()> {
    if config
        .overload
        .message_contains
        .iter()
        .any(|pattern| pattern.trim().is_empty())
    {
        return Err(Error::Invalid(
            "overload.message_contains must not contain empty strings".to_owned(),
        ));
    }
    if config.cutoff.max_in_a_row == 0 {
        return Err(Error::Invalid(
            "cutoff.max_in_a_row must be at least 1".to_owned(),
        ));
    }
    if config.overload.backoff_seconds.is_empty() || config.overload.backoff_seconds.contains(&0) {
        return Err(Error::Invalid(
            "overload.backoff_seconds must be a non-empty list of positive seconds".to_owned(),
        ));
    }
    if config.overload.jitter_percent > 100 {
        return Err(Error::Invalid(
            "overload.jitter_percent must be between 0 and 100".to_owned(),
        ));
    }
    if config.overload.max_total_minutes == 0 || config.overload.steady_seconds == 0 {
        return Err(Error::Invalid(
            "overload.steady_seconds and overload.max_total_minutes must be positive".to_owned(),
        ));
    }
    if config.limit.probe_minutes.is_empty() || config.limit.probe_minutes.contains(&0) {
        return Err(Error::Invalid(
            "limit.probe_minutes must be a non-empty list of positive minutes".to_owned(),
        ));
    }
    if config.limit.max_attempts == 0
        || config.limit.fallback_hours == 0
        || config.limit.verify_seconds == 0
    {
        return Err(Error::Invalid(
            "limit.max_attempts, limit.fallback_hours and limit.verify_seconds must be positive"
                .to_owned(),
        ));
    }
    let claude = &config.agents.claude;
    for (name, text) in [
        ("resume_message", &claude.resume_message),
        ("cutoff_message", &claude.cutoff_message),
        ("overload_message", &claude.overload_message),
    ] {
        if text.trim().is_empty() {
            return Err(Error::Invalid(format!(
                "agents.claude.{name} must not be empty"
            )));
        }
        let lowered = text.to_ascii_lowercase();
        if let Some(word) = DETECTOR_WORDS.iter().find(|word| lowered.contains(*word)) {
            return Err(Error::Invalid(format!(
                "agents.claude.{name} must not contain \"{word}\": the agent echoes it into the input line where detectors read it"
            )));
        }
    }
    if !config.notify.ntfy_topic.is_empty() && !config.notify.ntfy_server.starts_with("http") {
        return Err(Error::Invalid(
            "notify.ntfy_server must be an http(s) URL".to_owned(),
        ));
    }
    Ok(())
}

fn read_document(path: &Path) -> Result<Option<DocumentMut>> {
    let Some(bytes) = atomic_file::read_optional(path)? else {
        return Ok(None);
    };

    let text = String::from_utf8(bytes).map_err(|_| Error::Toml {
        path: path.to_path_buf(),
        detail: "not valid UTF-8".to_owned(),
    })?;
    let doc = text.parse::<DocumentMut>().map_err(|error| Error::Toml {
        path: path.to_path_buf(),
        detail: error.to_string(),
    })?;
    Ok(Some(doc))
}

fn remove_retired_keys(doc: &mut DocumentMut) {
    for (section, name) in RETIRED_KEYS {
        if let Some(table) = doc.get_mut(section).and_then(Item::as_table_like_mut) {
            table.remove(name);
        }
    }
}

fn is_retired_env(suffix: &str) -> bool {
    RETIRED_KEYS
        .iter()
        .any(|(section, name)| env_name(&format!("{section}.{name}")) == suffix)
}

fn defaults_document() -> Result<DocumentMut> {
    document_of(&Config::default())
        .map_err(|error| Error::Invalid(format!("default configuration: {error}")))
}

fn document_of(config: &Config) -> std::result::Result<DocumentMut, toml_edit::ser::Error> {
    let mut doc = toml_edit::ser::to_document(config)?;
    expand_inline_tables(doc.as_table_mut());
    Ok(doc)
}

fn expand_inline_tables(table: &mut Table) {
    let keys: Vec<String> = table.iter().map(|(name, _)| name.to_owned()).collect();
    for key in keys {
        let expanded = match table.get(&key) {
            Some(Item::Value(Value::InlineTable(inline))) => Some(inline.clone().into_table()),
            _ => None,
        };
        if let Some(mut inner) = expanded {
            expand_inline_tables(&mut inner);
            table[&key] = Item::Table(inner);
        } else if let Some(Item::Table(inner)) = table.get_mut(&key) {
            expand_inline_tables(inner);
        }
    }
}

fn flat_keys(table: &Table, prefix: &str) -> Vec<String> {
    let mut keys = Vec::new();
    for (name, item) in table.iter() {
        let key = if prefix.is_empty() {
            name.to_owned()
        } else {
            format!("{prefix}.{name}")
        };
        match item {
            Item::Table(inner) => keys.extend(flat_keys(inner, &key)),
            _ => keys.push(key),
        }
    }
    keys
}

fn lookup<'a>(table: &'a Table, key: &str) -> Option<&'a Item> {
    let (head, rest) = match key.split_once('.') {
        Some(parts) => parts,
        None => return table.get(key),
    };
    match table.get(head)? {
        Item::Table(inner) => lookup(inner, rest),
        _ => None,
    }
}

fn set_value(table: &mut Table, key: &str, value: Value) {
    match key.split_once('.') {
        Some((head, rest)) => {
            let entry = table
                .entry(head)
                .or_insert_with(|| Item::Table(Table::new()));
            if let Item::Table(inner) = entry {
                inner.set_implicit(true);
                set_value(inner, rest, value);
            }
        }
        None => {
            table[key] = Item::Value(value);
        }
    }
}

fn parse_env_value(raw: &str, template: &Value) -> std::result::Result<Value, String> {
    match template {
        Value::Boolean(_) => match raw.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "on" | "yes" => Ok(Value::from(true)),
            "0" | "false" | "off" | "no" => Ok(Value::from(false)),
            other => Err(format!("expected a boolean, got {other:?}")),
        },
        Value::Integer(_) => raw
            .trim()
            .parse::<i64>()
            .map(Value::from)
            .map_err(|_| format!("expected an integer, got {raw:?}")),
        Value::Array(array) => {
            let integer_elements = array.iter().next().is_some_and(|first| first.is_integer());
            let mut items = toml_edit::Array::new();
            for part in raw
                .split(',')
                .map(str::trim)
                .filter(|part| !part.is_empty())
            {
                if integer_elements {
                    let number = part
                        .parse::<i64>()
                        .map_err(|_| format!("expected integers, got {part:?}"))?;
                    items.push(number);
                } else {
                    items.push(part);
                }
            }
            Ok(Value::Array(items))
        }
        Value::String(_) => Ok(Value::from(raw)),
        other => Err(format!("unsupported value type {}", other.type_name())),
    }
}

fn render(item: &Item) -> String {
    match item {
        Item::Value(Value::String(text)) => text.value().clone(),
        Item::Value(value) => value.to_string().trim().to_owned(),
        other => other.to_string().trim().to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_overload_patterns_are_validated_and_preserve_builtins() {
        let config: Config =
            toml_edit::de::from_str("[overload]\nmessage_contains = ['pool exhausted']").unwrap();
        validate(&config).unwrap();
        assert!(config.overload.matches_message("POOL EXHAUSTED"));
        assert!(config.overload.matches_message(
            "only reserved subscriptions are left and their shared part is used up"
        ));
        assert!(
            !config
                .overload
                .matches_message("You've hit your usage limit")
        );
        let mut invalid = config;
        invalid.overload.message_contains.push("  ".to_owned());
        assert!(validate(&invalid).is_err());
    }

    #[test]
    fn defaults_load_without_a_file() {
        let paths = Paths::temp("config-defaults");
        let loaded = load_from(&paths, &[]).unwrap();
        assert_eq!(loaded.config.cutoff.max_in_a_row, 3);
        assert!(
            loaded
                .sources
                .values()
                .all(|source| *source == ValueSource::Default)
        );
        assert_eq!(
            get(&loaded, "notify.ntfy_server").unwrap(),
            "https://ntfy.sh"
        );
        std::fs::remove_dir_all(&paths.home).unwrap();
    }

    #[test]
    fn file_and_env_override_in_order() {
        let paths = Paths::temp("config-override");
        std::fs::write(
            &paths.config_file,
            "[cutoff]\nmax_in_a_row = 4\n\n[notify]\nntfy_privacy = \"terse\"\n",
        )
        .unwrap();
        let env = vec![
            (
                "CLAUDE_NONSTOP_CUTOFF_MAX_IN_A_ROW".to_owned(),
                "7".to_owned(),
            ),
            (
                "CLAUDE_NONSTOP_OVERLOAD_BACKOFF_SECONDS".to_owned(),
                "10, 20".to_owned(),
            ),
            ("CLAUDE_NONSTOP_ACTIVE".to_owned(), "1".to_owned()),
        ];
        let loaded = load_from(&paths, &env).unwrap();
        assert_eq!(loaded.config.cutoff.max_in_a_row, 7);
        assert_eq!(loaded.config.notify.ntfy_privacy, NtfyPrivacy::Terse);
        assert_eq!(loaded.config.overload.backoff_seconds, vec![10, 20]);
        assert_eq!(loaded.sources["cutoff.max_in_a_row"], ValueSource::Env);
        assert_eq!(loaded.sources["notify.ntfy_privacy"], ValueSource::File);
        assert_eq!(loaded.sources["limit.margin_seconds"], ValueSource::Default);
        std::fs::remove_dir_all(&paths.home).unwrap();
    }

    #[test]
    fn unknown_keys_and_bad_values_are_errors() {
        let paths = Paths::temp("config-errors");
        std::fs::write(&paths.config_file, "[notify]\nchanel = \"bell\"\n").unwrap();
        assert!(
            load_from(&paths, &[])
                .unwrap_err()
                .to_string()
                .contains("unknown key notify.chanel")
        );
        std::fs::write(&paths.config_file, "[cutoff]\nmax_in_a_row = 0\n").unwrap();
        assert!(
            load_from(&paths, &[])
                .unwrap_err()
                .to_string()
                .contains("max_in_a_row")
        );
        std::fs::remove_file(&paths.config_file).unwrap();
        let env = vec![(
            "CLAUDE_NONSTOP_CUTOFF_MAX_IN_A_ROW".to_owned(),
            "six".to_owned(),
        )];
        assert!(
            load_from(&paths, &env)
                .unwrap_err()
                .to_string()
                .contains("expected an integer")
        );
        let env = vec![("CLAUDE_NONSTOP_NOTIFY_LOUDNESS".to_owned(), "1".to_owned())];
        assert!(
            load_from(&paths, &env)
                .unwrap_err()
                .to_string()
                .contains("unknown configuration key")
        );
        std::fs::remove_dir_all(&paths.home).unwrap();
    }

    #[test]
    fn set_keeps_comments_and_rejects_detector_words() {
        let paths = Paths::temp("config-set");
        std::fs::write(
            &paths.config_file,
            "# my notes\n[general]\nauto_resume = true\n",
        )
        .unwrap();
        set(&paths, "notify.ntfy_privacy", "terse").unwrap();
        set(&paths, "agents.claude.enabled", "off").unwrap();
        let text = std::fs::read_to_string(&paths.config_file).unwrap();
        assert!(text.starts_with("# my notes\n"), "{text}");
        assert!(text.contains("ntfy_privacy = \"terse\""), "{text}");
        assert!(text.contains("[agents.claude]"), "{text}");
        let loaded = load_from(&paths, &[]).unwrap();
        assert_eq!(loaded.config.notify.ntfy_privacy, NtfyPrivacy::Terse);
        assert!(!loaded.config.agents.claude.enabled);
        let error = set(&paths, "agents.claude.overload_message", "please retry now").unwrap_err();
        assert!(error.to_string().contains("retry"), "{error}");
        assert!(
            set(&paths, "notify.ntfy_privacy", "loud")
                .unwrap_err()
                .to_string()
                .contains("ntfy_privacy")
        );
        std::fs::remove_dir_all(&paths.home).unwrap();
    }

    #[test]
    fn retired_keys_from_older_releases_are_ignored_and_dropped_on_set() {
        let paths = Paths::temp("config-retired");
        std::fs::write(&paths.config_file, "[notify]\nchannel = \"bell\"\n").unwrap();
        let env = vec![("CLAUDE_NONSTOP_NOTIFY_CHANNEL".to_owned(), "osc".to_owned())];
        let loaded = load_from(&paths, &env).unwrap();
        assert!(!loaded.sources.contains_key("notify.channel"));
        set(&paths, "notify.enabled", "off").unwrap();
        let text = std::fs::read_to_string(&paths.config_file).unwrap();
        assert!(!text.contains("channel"), "{text}");
        assert!(!load_from(&paths, &[]).unwrap().config.notify.enabled);
        std::fs::remove_dir_all(&paths.home).unwrap();
    }

    #[test]
    fn list_covers_every_key_with_its_source() {
        let paths = Paths::temp("config-list");
        let loaded = load_from(
            &paths,
            &[("CLAUDE_NONSTOP_NOTIFY_ENABLED".to_owned(), "0".to_owned())],
        )
        .unwrap();
        let rows = list(&loaded).unwrap();
        assert!(
            rows.iter()
                .any(|(key, value, source)| key == "notify.enabled"
                    && value == "false"
                    && *source == ValueSource::Env)
        );
        assert!(
            rows.iter()
                .any(|(key, value, _)| key == "overload.backoff_seconds"
                    && value == "[30, 60, 120, 240, 300]")
        );
        assert_eq!(rows.len(), loaded.sources.len());
        std::fs::remove_dir_all(&paths.home).unwrap();
    }
}
