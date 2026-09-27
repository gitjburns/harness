//! `config.toml` and `.env`, both read from the working directory.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, bail};
use serde::Deserialize;

const CONFIG_PATH: &str = "config.toml";
const ENV_PATH: &str = ".env";
const DEFAULT_OUTPUT_LIMIT: usize = 100_000;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    endpoint: Endpoint,
    #[serde(default)]
    chat: Chat,
    classifier: Classifier,
    #[serde(default)]
    repl: Repl,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Endpoint {
    /// OpenAI-compatible base URL, e.g. `https://api.openai.com/v1`.
    base_url: String,
    model: String,
    /// Name of the environment variable holding the API key. Omitted means the
    /// request is sent without an `Authorization` header.
    api_key_env: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Chat {
    /// System message sent ahead of the conversation in every chat request, never
    /// saved to the session. Omitted means no system message.
    prompt: Option<String>,
    /// Send earlier turns' reasoning as `reasoning()` REPL calls (`tool_reasoning`).
    /// Rewritten by `/tool-reasoning`.
    #[serde(default)]
    tool_reasoning: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Classifier {
    /// System prompt for the safety classifier. Required even when `approval_mode`
    /// isn't `auto`, since Shift+Tab can switch to it at any time.
    prompt: String,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Repl {
    #[serde(default)]
    approval_mode: ApprovalMode,
    /// Maximum bytes of REPL output sent to the model per call.
    output_limit: Option<usize>,
}

/// How REPL calls are approved.
#[derive(Deserialize, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalMode {
    /// Every call runs without asking.
    Allow,
    /// Every call asks the user.
    Ask,
    /// The classifier decides; inconclusive calls ask the user.
    #[default]
    Auto,
}

impl ApprovalMode {
    pub fn as_str(self) -> &'static str {
        match self {
            ApprovalMode::Allow => "allow",
            ApprovalMode::Ask => "ask",
            ApprovalMode::Auto => "auto",
        }
    }

    /// Shift+Tab order: ask → auto → allow → ask.
    pub fn next(self) -> Self {
        match self {
            ApprovalMode::Ask => ApprovalMode::Auto,
            ApprovalMode::Auto => ApprovalMode::Allow,
            ApprovalMode::Allow => ApprovalMode::Ask,
        }
    }
}

/// Endpoint settings resolved for sending requests.
#[derive(Clone)]
pub struct ResolvedEndpoint {
    pub url: String,
    pub model: String,
    pub api_key: Option<String>,
}

pub struct Settings {
    pub endpoint: ResolvedEndpoint,
    pub chat_prompt: Option<String>,
    pub tool_reasoning: bool,
    pub classifier_prompt: String,
    pub approval_mode: ApprovalMode,
    pub output_limit: usize,
}

pub fn load() -> anyhow::Result<Settings> {
    let text =
        std::fs::read_to_string(CONFIG_PATH).with_context(|| format!("reading {CONFIG_PATH}"))?;
    let config: Config = toml::from_str(&text).with_context(|| format!("parsing {CONFIG_PATH}"))?;

    // `.env` is read privately, never added to the process environment, so the REPL
    // (which inherits that environment) can't see its secrets. A missing `.env` is
    // fine.
    let mut dotenv = HashMap::new();
    if Path::new(ENV_PATH).exists() {
        for item in
            dotenvy::from_path_iter(ENV_PATH).with_context(|| format!("loading {ENV_PATH}"))?
        {
            let (key, value) = item.with_context(|| format!("parsing {ENV_PATH}"))?;
            dotenv.insert(key, value);
        }
    }

    let endpoint = config.endpoint;
    let api_key = match &endpoint.api_key_env {
        // Variables already set in the shell take precedence over `.env`.
        Some(name) => match std::env::var(name).ok().or_else(|| dotenv.remove(name)) {
            Some(key) => Some(key),
            None => bail!(
                "{CONFIG_PATH}: api_key_env names {name}, which is not set in the environment or {ENV_PATH}"
            ),
        },
        None => None,
    };

    Ok(Settings {
        endpoint: ResolvedEndpoint {
            url: format!(
                "{}/chat/completions",
                endpoint.base_url.trim_end_matches('/')
            ),
            model: endpoint.model,
            api_key,
        },
        chat_prompt: config.chat.prompt,
        tool_reasoning: config.chat.tool_reasoning,
        classifier_prompt: config.classifier.prompt,
        approval_mode: config.repl.approval_mode,
        output_limit: config.repl.output_limit.unwrap_or(DEFAULT_OUTPUT_LIMIT),
    })
}

/// Persist `approval_mode` (Shift+Tab).
pub fn save_approval_mode(mode: ApprovalMode) -> anyhow::Result<()> {
    save_setting("repl", "approval_mode", mode.as_str().into())
}

/// Persist `tool_reasoning` (`/tool-reasoning`).
pub fn save_tool_reasoning(enabled: bool) -> anyhow::Result<()> {
    save_setting("chat", "tool_reasoning", enabled.into())
}

/// Change only `[table] key` (creating the table if missing) so comments and
/// formatting elsewhere in the file are preserved.
fn save_setting(table: &str, key: &str, new: toml_edit::Value) -> anyhow::Result<()> {
    let text =
        std::fs::read_to_string(CONFIG_PATH).with_context(|| format!("reading {CONFIG_PATH}"))?;
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .with_context(|| format!("parsing {CONFIG_PATH}"))?;
    // Table-like covers both `[repl]` and `repl = { ... }`.
    let settings = doc
        .entry(table)
        .or_insert_with(toml_edit::table)
        .as_table_like_mut()
        .with_context(|| format!("{CONFIG_PATH}: `{table}` is not a table"))?;
    match settings
        .get_mut(key)
        .and_then(toml_edit::Item::as_value_mut)
    {
        // Replace the value in place, keeping its decor (e.g. a trailing comment).
        Some(value) => {
            let decor = value.decor().clone();
            *value = new;
            *value.decor_mut() = decor;
        }
        None => {
            settings.insert(key, toml_edit::Item::Value(new));
        }
    }

    // Temp file and rename, so a crash never leaves a truncated config.
    use std::io::Write;
    let tmp = format!("{CONFIG_PATH}.tmp");
    let mut file = std::fs::File::create(&tmp).with_context(|| format!("creating {tmp}"))?;
    file.write_all(doc.to_string().as_bytes())
        .and_then(|()| file.sync_all())
        .with_context(|| format!("writing {tmp}"))?;
    std::fs::rename(&tmp, CONFIG_PATH).with_context(|| format!("renaming {tmp} to {CONFIG_PATH}"))
}
