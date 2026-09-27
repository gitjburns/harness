//! Chat completions: the streaming chat request (over server-sent events) and the
//! non-streaming classifier request.

use anyhow::{Context, bail};
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{Map, Number, Value, json};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;

use crate::config::ResolvedEndpoint;

/// The only tool the model has. Sent with every chat request.
fn repl_tool() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "REPL",
            "description": "Execute Python in a REPL session. State persists for the lifetime of the current turn — variables and data survive across the tool calls you make during the current turn, but never carry over to a later turn. Call help() to see the available functions and libraries.",
            "parameters": {
                "type": "object",
                "properties": {
                    "code": {
                        "type": "string",
                        "description": "The Python source to execute."
                    }
                },
                "required": ["code"]
            }
        }
    })
}

pub enum StreamEvent {
    /// One chunk's `delta`, every field as sent. The reply is assembled from these
    /// with `accumulate`.
    Delta(Map<String, Value>),
    /// Usage from the final usage chunk: `total` is prompt + completion, `prompt` is
    /// the prompt alone, sent back to the model as the next turn's FYI count.
    Usage { total: u64, prompt: Option<u64> },
    /// The reply finished normally. `truncated` means it stopped at the token limit
    /// (`finish_reason: "length"`), so a tool call may have been cut off mid-stream.
    Done { truncated: bool },
    /// The request or stream failed. Deltas already sent remain part of the reply.
    Error(String),
}

/// Start a streaming request. Each request gets its own channel, so aborting the
/// task and dropping the receiver discards everything from that request.
pub fn start(
    http: &reqwest::Client,
    endpoint: &ResolvedEndpoint,
    system_prompt: Option<&str>,
    messages: Vec<Value>,
) -> (JoinHandle<()>, UnboundedReceiver<StreamEvent>) {
    // The system message comes from config at send time, like `tools`; it is never
    // part of the session, so the current config applies to resumed sessions too.
    let messages: Vec<Value> = system_prompt
        .map(|prompt| json!({ "role": "system", "content": prompt }))
        .into_iter()
        .chain(messages)
        .collect();
    let body = json!({
        "model": endpoint.model,
        "stream": true,
        "stream_options": { "include_usage": true },
        "tools": [repl_tool()],
        "messages": messages,
    });
    let request = post(http, endpoint, &body);

    let (tx, rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        let event = match stream(request, &tx).await {
            Ok(truncated) => StreamEvent::Done { truncated },
            Err(e) => StreamEvent::Error(format!("{e:#}")),
        };
        let _ = tx.send(event);
    });
    (task, rx)
}

async fn stream(
    request: reqwest::RequestBuilder,
    tx: &UnboundedSender<StreamEvent>,
) -> anyhow::Result<bool> {
    let response = request.send().await.context("sending request")?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        bail!("HTTP {status}: {body}");
    }

    let mut bytes = response.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    let mut finish_reason: Option<String> = None;
    while let Some(chunk) = bytes.next().await {
        buf.extend_from_slice(&chunk.context("reading stream")?);
        // Split on raw `\n` bytes; this never lands inside a UTF-8 sequence, and
        // multi-byte characters split across chunks stay buffered until complete.
        while let Some(end) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=end).collect();
            let line = String::from_utf8_lossy(&line);
            // Only `data:` lines matter; comments, `event:` lines, and the blank
            // lines separating events are ignored.
            let Some(data) = line.trim_end().strip_prefix("data:") else {
                continue;
            };
            let data = data.trim_start();
            if data == "[DONE]" {
                return Ok(finish_reason.as_deref() == Some("length"));
            }
            let mut chunk: Value = serde_json::from_str(data)
                .with_context(|| format!("parsing stream data: {data}"))?;
            if let Some(error) = chunk.get("error") {
                bail!("{error}");
            }
            if let Some(reason) = chunk["choices"][0]["finish_reason"].as_str() {
                finish_reason = Some(reason.to_string());
            }
            // The final usage chunk has no choices, hence `pointer_mut`.
            if let Some(Value::Object(delta)) =
                chunk.pointer_mut("/choices/0/delta").map(Value::take)
                && !delta.is_empty()
                && tx.send(StreamEvent::Delta(delta)).is_err()
            {
                return Ok(false); // Receiver dropped: the request was cancelled.
            }
            if let Some(total) = chunk["usage"]["total_tokens"].as_u64()
                && tx
                    .send(StreamEvent::Usage {
                        total,
                        prompt: chunk["usage"]["prompt_tokens"].as_u64(),
                    })
                    .is_err()
            {
                return Ok(false);
            }
        }
    }
    // Some servers close the stream after the final chunk without sending `[DONE]`.
    match finish_reason {
        Some(reason) => Ok(reason == "length"),
        None => bail!("stream ended before the reply finished"),
    }
}

/// Merge a streamed `delta` into the message assembled so far, by the OpenAI SDK's
/// rule (`accumulate_delta`): strings are appended, numbers added, objects merged
/// recursively, and lists of entries with an `index` merged entry by entry. `index`
/// and `type` are replaced instead, since fragments repeat them. The rule doesn't
/// depend on field names, so fields the app doesn't know are reassembled too.
pub fn accumulate(acc: &mut Map<String, Value>, delta: Map<String, Value>) {
    for (key, value) in delta {
        let slot = acc.entry(key.as_str()).or_insert(Value::Null);
        // Even the first indexed list is merged: one chunk can hold several
        // fragments of the same entry.
        if slot.is_null()
            && let Value::Array(entries) = &value
            && has_indexed_entries(entries)
        {
            *slot = Value::Array(Vec::new());
        }
        if slot.is_null() || key == "index" || key == "type" {
            *slot = value;
            continue;
        }
        match (slot, value) {
            (Value::String(acc), Value::String(delta)) => acc.push_str(&delta),
            (Value::Number(acc), Value::Number(delta)) => *acc = add(acc, &delta),
            (Value::Object(acc), Value::Object(delta)) => accumulate(acc, delta),
            (Value::Array(acc), Value::Array(delta)) => accumulate_entries(acc, delta),
            // Mismatched types, such as a later `null`: keep what was assembled.
            _ => {}
        }
    }
}

/// Lists of plain values only gain entries. Otherwise each fragment merges into the
/// entry with the same `index`. Unlike the SDK, entries are matched by their `index`
/// value rather than list position (so out-of-order indices can't merge into the
/// wrong entry), and a fragment without one is appended instead of failing the reply.
fn accumulate_entries(acc: &mut Vec<Value>, delta: Vec<Value>) {
    let plain = acc.iter().all(|v| v.is_string() || v.is_number());
    if plain && (!acc.is_empty() || !has_indexed_entries(&delta)) {
        acc.extend(delta);
        return;
    }
    for fragment in delta {
        let target = fragment
            .get("index")
            .filter(|index| index.is_u64())
            .and_then(|index| {
                acc.iter()
                    .position(|entry| entry.get("index") == Some(index))
            });
        match (target, fragment) {
            (Some(position), Value::Object(fragment)) => {
                if let Value::Object(entry) = &mut acc[position] {
                    accumulate(entry, fragment);
                }
            }
            (_, fragment) => acc.push(fragment),
        }
    }
}

fn has_indexed_entries(entries: &[Value]) -> bool {
    entries
        .iter()
        .any(|entry| entry.get("index").is_some_and(Value::is_u64))
}

fn add(acc: &Number, delta: &Number) -> Number {
    match (acc.as_i64(), delta.as_i64()) {
        (Some(acc), Some(delta)) => acc.saturating_add(delta).into(),
        _ => Number::from_f64(acc.as_f64().unwrap_or(0.0) + delta.as_f64().unwrap_or(0.0))
            .unwrap_or_else(|| acc.clone()),
    }
}

fn post(
    http: &reqwest::Client,
    endpoint: &ResolvedEndpoint,
    body: &Value,
) -> reqwest::RequestBuilder {
    let request = http.post(&endpoint.url).json(body);
    match &endpoint.api_key {
        Some(key) => request.bearer_auth(key),
        None => request,
    }
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum VerdictKind {
    Safe,
    Unsafe,
    Inconclusive,
}

#[derive(Deserialize)]
pub struct Verdict {
    pub effects: String,
    pub verdict: VerdictKind,
}

/// Ask the classifier to judge `code`. It gets no conversation context: only the
/// configured system prompt, the repository root, and the code.
pub async fn classify(
    http: reqwest::Client,
    endpoint: &ResolvedEndpoint,
    prompt: &str,
    repo_root: &str,
    code: &str,
) -> anyhow::Result<Verdict> {
    // `effects` precedes `verdict` so the model states what the code does before
    // judging it; strict mode requires every property in `required`.
    let schema = json!({
        "type": "object",
        "properties": {
            "effects": {
                "type": "string",
                "description": "The code's effects relevant to the rules: files read, written, deleted, or renamed; commands run; network access."
            },
            "verdict": { "type": "string", "enum": ["safe", "unsafe", "inconclusive"] }
        },
        "required": ["effects", "verdict"],
        "additionalProperties": false
    });
    let body = json!({
        "model": endpoint.model,
        "messages": [
            { "role": "system", "content": prompt },
            { "role": "user", "content": format!("Repository root: {repo_root}\n\nCode:\n```python\n{code}\n```") },
        ],
        "response_format": {
            "type": "json_schema",
            "json_schema": { "name": "verdict", "strict": true, "schema": schema }
        },
    });
    let response = post(&http, endpoint, &body)
        .send()
        .await
        .context("sending classifier request")?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        bail!("HTTP {status}: {body}");
    }
    let reply: Value = response
        .json()
        .await
        .context("reading classifier response")?;
    let content = reply["choices"][0]["message"]["content"]
        .as_str()
        .context("classifier response has no content")?;
    serde_json::from_str(content).with_context(|| format!("parsing classifier verdict: {content}"))
}
