//! Chat completions: the streaming chat request (over server-sent events) and the
//! non-streaming classifier request.

use anyhow::{Context, bail};
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;

use crate::config::ResolvedEndpoint;
use crate::session::Message;

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
    Content(String),
    Reasoning(String),
    /// A fragment of tool call `index`. The id and name arrive once, typically in the
    /// first fragment; `arguments` is a piece of the JSON arguments string.
    ToolCall {
        index: usize,
        id: Option<String>,
        name: Option<String>,
        arguments: String,
    },
    /// `total_tokens` (prompt + completion) from the usage chunk the server sends
    /// after the final choice chunk.
    Usage(u64),
    /// The reply finished normally. `truncated` means it stopped at the token limit
    /// (`finish_reason: "length"`), so a tool call may have been cut off mid-stream.
    Done {
        truncated: bool,
    },
    /// The request or stream failed. Deltas already sent remain part of the reply.
    Error(String),
}

/// Start a streaming request. Each request gets its own channel, so aborting the
/// task and dropping the receiver discards everything from that request.
pub fn start(
    http: &reqwest::Client,
    endpoint: &ResolvedEndpoint,
    messages: &[Message],
) -> (JoinHandle<()>, UnboundedReceiver<StreamEvent>) {
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
            let chunk: Value = serde_json::from_str(data)
                .with_context(|| format!("parsing stream data: {data}"))?;
            if let Some(error) = chunk.get("error") {
                bail!("{error}");
            }
            let choice = &chunk["choices"][0];
            let delta = &choice["delta"];
            let events = [
                (
                    "reasoning",
                    StreamEvent::Reasoning as fn(String) -> StreamEvent,
                ),
                ("content", StreamEvent::Content),
            ];
            for (field, event) in events {
                if let Some(text) = delta[field].as_str()
                    && !text.is_empty()
                    && tx.send(event(text.to_string())).is_err()
                {
                    return Ok(false); // Receiver dropped: the request was cancelled.
                }
            }
            for call in delta["tool_calls"].as_array().into_iter().flatten() {
                let event = StreamEvent::ToolCall {
                    index: call["index"].as_u64().unwrap_or(0) as usize,
                    id: call["id"].as_str().map(str::to_string),
                    name: call["function"]["name"].as_str().map(str::to_string),
                    arguments: call["function"]["arguments"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                };
                if tx.send(event).is_err() {
                    return Ok(false);
                }
            }
            if let Some(reason) = choice["finish_reason"].as_str() {
                finish_reason = Some(reason.to_string());
            }
            if let Some(total) = chunk["usage"]["total_tokens"].as_u64()
                && tx.send(StreamEvent::Usage(total)).is_err()
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
