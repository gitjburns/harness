//! Session file: `{ "messages": [...] }`, where `messages` is exactly the array sent
//! to the endpoint. Messages are appended, except that resume inserts results for
//! unanswered tool calls, and each turn removes earlier `FYI()`/`help()`-only calls
//! (`remove_calls`). Partial replies are kept as-is, except that a tool call still
//! streaming when the reply ended is dropped.

use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::Context;
use serde::{Deserialize, Serialize};

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
    /// Assistant reasoning, sent back to the endpoint as-is. The field name is what
    /// vLLM streams (`delta.reasoning`) and accepts on input messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    /// Complete tool calls made by an assistant message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    /// The call a `tool` message answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
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
}

#[derive(Serialize, Deserialize, Clone)]
pub struct ToolCall {
    pub id: String,
    /// Always `"function"`.
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionCall,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct FunctionCall {
    pub name: String,
    /// JSON-encoded arguments, exactly as the model produced them.
    pub arguments: String,
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

impl Session {
    /// A new session. The file is not created until the first `save`, so a launch
    /// with no messages leaves nothing behind.
    pub fn new() -> Self {
        let name = chrono::Local::now().format("%Y%m%d-%H%M%S");
        Session {
            path: PathBuf::from(SESSIONS_DIR).join(format!("{name}.json")),
            messages: Vec::new(),
        }
    }

    /// Load a session. Tool calls left without results (the app was killed or
    /// crashed mid-turn) get `NOT_RUN` results, saved immediately, since the endpoint
    /// rejects every request while any are missing. Returns how many were added.
    pub fn resume(path: PathBuf) -> anyhow::Result<(Self, usize)> {
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

    /// Remove every tool call matching `matches`, with its tool result, and save if
    /// anything changed. An assistant message left with no calls, content, or
    /// reasoning is removed too; one that still has any of them stays without the
    /// call. Returns how many calls were removed.
    pub fn remove_calls(&mut self, matches: impl Fn(&ToolCall) -> bool) -> anyhow::Result<usize> {
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
                *emptied = message.content.is_empty() && message.reasoning.is_none();
            }
        }
        if removed_ids.is_empty() {
            return Ok(0);
        }
        let mut emptied = emptied.into_iter();
        self.messages.retain(|message| {
            let emptied = emptied.next().unwrap_or(false);
            let orphaned_result = message.role == Role::Tool
                && message
                    .tool_call_id
                    .as_ref()
                    .is_some_and(|id| removed_ids.contains(id));
            !emptied && !orphaned_result
        });
        self.save()?;
        Ok(removed_ids.len())
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
