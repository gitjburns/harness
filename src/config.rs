//! `config.toml` and `.env`, both read from the harness directory (`harness_dir`).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::Deserialize;

const CONFIG_FILE: &str = "config.toml";
const ENV_FILE: &str = ".env";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    endpoint: Endpoint,
    chat: Chat,
    repl: Repl,
    commands: Commands,
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Chat {
    /// System message sent ahead of the conversation in every chat request, never
    /// saved to the session. Omitted means no system message.
    prompt: Option<String>,
    /// Send earlier turns' reasoning as `reasoning()` REPL calls (`tool_reasoning`).
    /// Rewritten by `/tool-reasoning`.
    tool_reasoning: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Commands {
    /// Denied names take precedence over allowed names when commands are brokered.
    pub deny: Vec<String>,
    /// Exact environment variable names withheld from command processes.
    pub env_filter: Vec<String>,
    /// Model-facing command names mapped to absolute executable paths.
    pub allow: BTreeMap<String, PathBuf>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Repl {
    /// Model-facing tool description, required and sent verbatim on every request.
    tool_description: String,
    permission: Permission,
    /// Worker memory limit in MiB; conversion to bytes belongs to the runtime.
    max_memory_mb: usize,
    /// Maximum bytes of REPL output sent to the model per call.
    output_limit: usize,
}

/// REPL filesystem access inside the repository; outside access is denied at every level.
#[derive(Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Permission {
    /// Deny repository reads and writes.
    None,
    /// Allow repository reads, deny writes.
    ReadOnly,
    /// Allow repository reads and writes.
    ReadWrite,
}

impl Permission {
    /// The configuration spelling, also shown in the status line.
    pub fn as_str(self) -> &'static str {
        match self {
            Permission::None => "none",
            Permission::ReadOnly => "read-only",
            Permission::ReadWrite => "read-write",
        }
    }

    /// Shift+Tab order: none → read-only → read-write → none.
    pub fn next(self) -> Self {
        match self {
            Permission::None => Permission::ReadOnly,
            Permission::ReadOnly => Permission::ReadWrite,
            Permission::ReadWrite => Permission::None,
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
    /// The harness directory the settings were loaded from (`harness_dir`).
    pub dir: PathBuf,
    pub endpoint: ResolvedEndpoint,
    pub chat_prompt: Option<String>,
    pub tool_reasoning: bool,
    pub repl_tool_description: String,
    pub permission: Permission,
    pub max_memory_mb: usize,
    pub commands: Commands,
    pub output_limit: usize,
}

/// `~/.harness`: config, `.env`, sessions, and `replib/`, shared by every directory the
/// app runs in.
pub fn harness_dir() -> anyhow::Result<PathBuf> {
    std::env::home_dir()
        .map(|home| home.join(".harness"))
        .context("can't find the home directory")
}

/// Require explicit operational settings and resolve credentials without exporting secrets.
pub fn load(dir: &Path) -> anyhow::Result<Settings> {
    let config_path = dir.join(CONFIG_FILE);
    let text = std::fs::read_to_string(&config_path)
        .with_context(|| format!("reading {}", config_path.display()))?;
    let config: Config =
        toml::from_str(&text).with_context(|| format!("parsing {}", config_path.display()))?;

    // Command execution must use the configured executable, never resolve an allow
    // entry relative to the repository or through PATH.
    for (name, path) in &config.commands.allow {
        if !path.is_absolute() {
            bail!(
                "{}: commands.allow.{name} must be an absolute path, got {}",
                config_path.display(),
                path.display()
            );
        }
    }

    // `.env` is read privately, never exported to the app's environment or its
    // child processes. A missing `.env` is fine.
    let env_path = dir.join(ENV_FILE);
    let mut dotenv = HashMap::new();
    if env_path.exists() {
        for item in dotenvy::from_path_iter(&env_path)
            .with_context(|| format!("loading {}", env_path.display()))?
        {
            let (key, value) = item.with_context(|| format!("parsing {}", env_path.display()))?;
            dotenv.insert(key, value);
        }
    }

    let endpoint = config.endpoint;
    let api_key = match &endpoint.api_key_env {
        // Variables already set in the shell take precedence over `.env`.
        Some(name) => match std::env::var(name).ok().or_else(|| dotenv.remove(name)) {
            Some(key) => Some(key),
            None => bail!(
                "{}: api_key_env names {name}, which is not set in the environment or {}",
                config_path.display(),
                env_path.display()
            ),
        },
        None => None,
    };

    Ok(Settings {
        dir: dir.to_path_buf(),
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
        repl_tool_description: config.repl.tool_description,
        permission: config.repl.permission,
        max_memory_mb: config.repl.max_memory_mb,
        commands: config.commands,
        output_limit: config.repl.output_limit,
    })
}

/// Persist `permission` (Shift+Tab).
pub fn save_permission(dir: &Path, permission: Permission) -> anyhow::Result<()> {
    save_setting(dir, "repl", "permission", permission.as_str().into())
}

/// Persist `tool_reasoning` (`/tool-reasoning`).
pub fn save_tool_reasoning(dir: &Path, enabled: bool) -> anyhow::Result<()> {
    save_setting(dir, "chat", "tool_reasoning", enabled.into())
}

/// Change only `[table] key` (creating the table if missing) so comments and
/// formatting elsewhere in the file are preserved.
fn save_setting(dir: &Path, table: &str, key: &str, new: toml_edit::Value) -> anyhow::Result<()> {
    let config_path = dir.join(CONFIG_FILE);
    let text = std::fs::read_to_string(&config_path)
        .with_context(|| format!("reading {}", config_path.display()))?;
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .with_context(|| format!("parsing {}", config_path.display()))?;
    // Table-like covers both `[repl]` and `repl = { ... }`.
    let settings = doc
        .entry(table)
        .or_insert_with(toml_edit::table)
        .as_table_like_mut()
        .with_context(|| format!("{}: `{table}` is not a table", config_path.display()))?;
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
    let tmp = config_path.with_extension("toml.tmp");
    let mut file =
        std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    file.write_all(doc.to_string().as_bytes())
        .and_then(|()| file.sync_all())
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &config_path)
        .with_context(|| format!("renaming {} to {}", tmp.display(), config_path.display()))
}
