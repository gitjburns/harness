//! Session file: `{ "messages": [...] }`, where `messages` is exactly the array sent
//! to the endpoint after the configured system message, which is never saved.
//! Messages are appended, except that resume inserts results for
//! unanswered tool calls, and each turn removes earlier `FYI()`/`help()`-only calls
//! (`remove_calls`). Partial replies are kept as-is, except that a tool call still
//! streaming when the reply ended is dropped. Assistant messages keep every field
//! the endpoint streamed, including ones the app doesn't know.

use std::borrow::Cow;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Sessions directory, inside the harness directory. A session's name is its file
/// name without `.json`.
const SESSIONS_DIR: &str = "sessions";

/// Result recorded for a complete tool call that never ran.
pub const NOT_RUN: &str = "[not run: turn stopped]";

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    #[default]
    User,
    Assistant,
    Tool,
}

#[derive(Serialize, Deserialize, Default)]
pub struct Message {
    pub role: Role,
    pub content: String,
    /// Complete tool calls made by an assistant message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    /// The call a `tool` message answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Every other field of an assistant message, exactly as the endpoint streamed
    /// it: reasoning under whatever name the endpoint uses, and fields the app doesn't
    /// know. Sent back unchanged, so the endpoint gets its own format.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Message {
    pub fn user(content: String) -> Self {
        Message {
            role: Role::User,
            content,
            ..Default::default()
        }
    }

    pub fn tool(tool_call_id: String, content: String) -> Self {
        Message {
            role: Role::Tool,
            content,
            tool_call_id: Some(tool_call_id),
            ..Default::default()
        }
    }

    /// Reasoning to display, from whichever field the endpoint used.
    pub fn reasoning(&self) -> Option<Cow<'_, str>> {
        reasoning_text(&self.extra)
    }

    /// Nothing worth keeping: no content, no calls, and no other field with a value.
    pub fn is_empty(&self) -> bool {
        self.content.is_empty()
            && self.tool_calls.as_ref().is_none_or(Vec::is_empty)
            && !self.extra.values().any(has_value)
    }
}

/// Display text of the reasoning in an assistant message's fields: `reasoning`, else
/// `reasoning_content`, else the text of the `reasoning_details` blocks (encrypted
/// blocks have none). Endpoints that send more than one carry the same reasoning in
/// each, so only one is shown.
pub fn reasoning_text(fields: &Map<String, Value>) -> Option<Cow<'_, str>> {
    for key in ["reasoning", "reasoning_content"] {
        if let Some(text) = fields.get(key).and_then(Value::as_str)
            && !text.is_empty()
        {
            return Some(Cow::Borrowed(text));
        }
    }
    let text = fields
        .get("reasoning_details")?
        .as_array()?
        .iter()
        .filter_map(|block| block.get("text").or_else(|| block.get("summary"))?.as_str())
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    (!text.is_empty()).then_some(Cow::Owned(text))
}

/// Streams often carry placeholder fields (`"refusal": null`, `"reasoning": ""`);
/// those alone don't make a message worth keeping.
fn has_value(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
        Value::Bool(_) | Value::Number(_) => true,
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct ToolCall {
    pub id: String,
    /// Always `"function"`.
    #[serde(rename = "type", default = "function_kind")]
    pub kind: String,
    pub function: FunctionCall,
    /// Fields the app doesn't use, kept as streamed and sent back unchanged.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn function_kind() -> String {
    "function".to_string()
}

#[derive(Serialize, Deserialize, Clone)]
pub struct FunctionCall {
    pub name: String,
    /// JSON-encoded arguments, exactly as the model produced them.
    #[serde(default)]
    pub arguments: String,
    /// Fields the app doesn't use, kept as streamed and sent back unchanged.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Deserialize)]
struct SessionFile {
    messages: Vec<Message>,
}

/// Serialization twin of `SessionFile` that borrows the messages.
#[derive(Serialize)]
struct SessionFileRef<'a> {
    messages: &'a [Message],
}

pub struct Session {
    pub path: PathBuf,
    pub messages: Vec<Message>,
}

/// Session file name for `name`. Names must stay inside the sessions directory and
/// not collide with hidden or temp files: not empty, no `/`, no leading `.`.
fn file_name(name: &str) -> anyhow::Result<String> {
    if name.is_empty() || name.contains('/') || name.starts_with('.') {
        bail!("invalid session name: {name:?}");
    }
    Ok(format!("{name}.json"))
}

/// Names of saved sessions, least recently changed first. No sessions directory
/// means no sessions.
pub fn list(harness_dir: &Path) -> anyhow::Result<Vec<String>> {
    let dir = harness_dir.join(SESSIONS_DIR);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("reading {}", dir.display())),
    };
    let mut sessions = Vec::new();
    for entry in entries {
        let path = entry
            .with_context(|| format!("reading {}", dir.display()))?
            .path();
        // Skips `.json.tmp` files left by an interrupted save.
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let modified = std::fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .with_context(|| format!("reading {}", path.display()))?;
        sessions.push((modified, name.to_string()));
    }
    sessions.sort();
    Ok(sessions.into_iter().map(|(_, name)| name).collect())
}

impl Session {
    /// A new session, named by its start time. The file is not created until the
    /// first `save`, so a launch with no messages leaves nothing behind.
    pub fn new(harness_dir: &Path) -> Self {
        let name = chrono::Local::now().format("%Y%m%d-%H%M%S");
        Session {
            path: harness_dir.join(SESSIONS_DIR).join(format!("{name}.json")),
            messages: Vec::new(),
        }
    }

    /// Load session `name`. Tool calls left without results (the app was killed or
    /// crashed mid-turn) get `NOT_RUN` results, saved immediately, since the endpoint
    /// rejects every request while any are missing. Returns how many were added.
    pub fn resume(harness_dir: &Path, name: &str) -> anyhow::Result<(Self, usize)> {
        let path = harness_dir.join(SESSIONS_DIR).join(file_name(name)?);
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let file: SessionFile =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        let mut session = Session {
            path,
            messages: file.messages,
        };
        let repaired = session.answer_unanswered_calls();
        if repaired > 0 {
            session.save()?;
        }
        Ok((session, repaired))
    }

    /// Insert a `NOT_RUN` result for each tool call without one, after the tool
    /// messages that follow its assistant message.
    fn answer_unanswered_calls(&mut self) -> usize {
        let mut added = 0;
        let mut i = 0;
        while i < self.messages.len() {
            let Some(calls) = &self.messages[i].tool_calls else {
                i += 1;
                continue;
            };
            let ids: Vec<String> = calls.iter().map(|c| c.id.clone()).collect();
            let mut end = i + 1;
            let mut answered = HashSet::new();
            while let Some(message) = self.messages.get(end)
                && message.role == Role::Tool
            {
                answered.extend(message.tool_call_id.clone());
                end += 1;
            }
            let missing: Vec<Message> = ids
                .into_iter()
                .filter(|id| !answered.contains(id))
                .map(|id| Message::tool(id, NOT_RUN.to_string()))
                .collect();
            added += missing.len();
            let inserted = missing.len();
            self.messages.splice(end..end, missing);
            i = end + inserted;
        }
        added
    }

    pub fn push(&mut self, message: Message) -> anyhow::Result<()> {
        self.messages.push(message);
        self.save()
    }

    /// `/rename`: rename the session file, or before the first save, the file it will
    /// be created as. An existing session is never overwritten.
    pub fn rename(&mut self, name: &str) -> anyhow::Result<()> {
        let path = self.path.with_file_name(file_name(name)?);
        if path == self.path {
            return Ok(());
        }
        if path.exists() {
            bail!("session {name} already exists");
        }
        if self.path.exists() {
            std::fs::rename(&self.path, &path).with_context(|| {
                format!("renaming {} to {}", self.path.display(), path.display())
            })?;
        }
        self.path = path;
        Ok(())
    }

    /// Remove every tool call matching `matches`, with its tool result, and save if
    /// anything changed. An assistant message left empty (`Message::is_empty`) is
    /// removed too; one that still has content or another field stays without the
    /// call. Returns the pre-removal indices of the removed messages, ascending.
    pub fn remove_calls(
        &mut self,
        matches: impl Fn(&ToolCall) -> bool,
    ) -> anyhow::Result<Vec<usize>> {
        let mut removed_ids = HashSet::new();
        let mut emptied = vec![false; self.messages.len()];
        for (message, emptied) in self.messages.iter_mut().zip(&mut emptied) {
            let Some(calls) = &mut message.tool_calls else {
                continue;
            };
            calls.retain(|call| {
                let remove = matches(call);
                if remove {
                    removed_ids.insert(call.id.clone());
                }
                !remove
            });
            if calls.is_empty() {
                message.tool_calls = None;
                *emptied = message.is_empty();
            }
        }
        if removed_ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut removed = Vec::new();
        let mut index = 0;
        let mut emptied = emptied.into_iter();
        self.messages.retain(|message| {
            let emptied = emptied.next().unwrap_or(false);
            let orphaned_result = message.role == Role::Tool
                && message
                    .tool_call_id
                    .as_ref()
                    .is_some_and(|id| removed_ids.contains(id));
            let keep = !emptied && !orphaned_result;
            if !keep {
                removed.push(index);
            }
            index += 1;
            keep
        });
        self.save()?;
        Ok(removed)
    }

    /// Write via a synced temp file and rename so a crash or power loss never leaves a
    /// half-written file.
    fn save(&self) -> anyhow::Result<()> {
        use std::io::Write;

        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let json = serde_json::to_string_pretty(&SessionFileRef {
            messages: &self.messages,
        })?;
        let tmp = self.path.with_extension("json.tmp");
        let mut file =
            std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        file.write_all(json.as_bytes())
            .and_then(|()| file.sync_all())
            .with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("renaming {} to {}", tmp.display(), self.path.display()))
    }
}
