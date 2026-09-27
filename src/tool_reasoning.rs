//! Tool reasoning (`[chat] tool_reasoning`): an experimental rewrite of what the model
//! sees. Each assistant message with reasoning in an earlier turn is sent as a
//! synthetic `REPL` call `reasoning("<text>")` with its `(no output)` result, followed
//! by the message without its native reasoning fields. The turn in progress keeps its
//! native reasoning. Applied when requests and the transcript are built; the session
//! file is never changed, so the toggle can be switched either way at any time.
//!
//! Honesty rule: the synthetic result claims what `reasoning()` produces, so it relies
//! on `replib/reasoning.py` defining it as a no-op that prints nothing.

use std::borrow::Cow;

use serde_json::{Value, json};

use crate::session::{Message, Role};

/// Native reasoning fields removed from a converted message.
const REASONING_FIELDS: [&str; 3] = ["reasoning", "reasoning_content", "reasoning_details"];

/// What `reasoning()` produces: nothing, which the REPL reports this way. Also the
/// result of every exact `reasoning("…")` call, which is answered without running.
pub const RESULT: &str = "(no output)";

/// Index of the turn in progress's user message. Messages before it are converted.
pub fn turn_start(messages: &[Message]) -> usize {
    messages
        .iter()
        .rposition(|message| message.role == Role::User)
        .unwrap_or(0)
}

/// The reasoning text converted for message `index`, if it is converted. Messages
/// with no reasoning text (only encrypted `reasoning_details`) keep their native
/// fields.
pub fn converted<'a>(
    messages: &'a [Message],
    index: usize,
    turn_start: usize,
) -> Option<Cow<'a, str>> {
    let message = &messages[index];
    if index >= turn_start || message.role != Role::Assistant {
        return None;
    }
    message.reasoning()
}

/// Synthetic call id. Derived from the message's position so it's the same in every
/// request built from the same messages; it only needs to be unique within one.
pub fn call_id(index: usize) -> String {
    format!("reasoning-{index}")
}

/// The synthetic call's code. A JSON string literal is also a valid Python one, so
/// `reasoning()` receives exactly the original text.
pub fn code(text: &str) -> String {
    format!("reasoning({})", Value::from(text))
}

/// The messages as sent, converted when `enabled`.
pub fn request_messages(messages: &[Message], enabled: bool) -> Vec<Value> {
    let turn_start = turn_start(messages);
    let mut sent = Vec::with_capacity(messages.len());
    for (index, message) in messages.iter().enumerate() {
        let text = if enabled {
            converted(messages, index, turn_start)
        } else {
            None
        };
        let Some(text) = text else {
            sent.push(json!(message));
            continue;
        };
        let id = call_id(index);
        sent.push(json!({
            "role": "assistant",
            "content": "",
            "tool_calls": [{
                "id": id,
                "type": "function",
                "function": {
                    "name": "REPL",
                    "arguments": json!({ "code": code(&text) }).to_string(),
                },
            }],
        }));
        sent.push(json!({ "role": "tool", "tool_call_id": id, "content": RESULT }));
        let mut message = json!(message);
        if let Some(fields) = message.as_object_mut() {
            for field in REASONING_FIELDS {
                fields.remove(field);
            }
        }
        sent.push(message);
    }
    sent
}

/// Code that is exactly one `reasoning(...)` call on a single Python string literal,
/// so nothing else can run. f-strings (which evaluate expressions), bytes, implicit
/// concatenation, keyword arguments, and any other code alongside don't qualify.
pub fn is_reasoning_call(code: &str) -> bool {
    code.trim()
        .strip_prefix("reasoning(")
        .and_then(|rest| rest.strip_suffix(')'))
        .is_some_and(|argument| is_string_literal(argument.trim()))
}

/// One complete Python string literal: an optional `r` or `u` prefix, then single,
/// double, or triple quotes. The first unescaped closing quote must end the text.
fn is_string_literal(text: &str) -> bool {
    let body = text.strip_prefix(['r', 'R', 'u', 'U']).unwrap_or(text);
    let quote = ["\"\"\"", "'''", "\"", "'"]
        .into_iter()
        .find(|quote| body.starts_with(quote));
    let Some(quote) = quote else {
        return false;
    };
    let content = &body[quote.len()..];
    let mut chars = content.char_indices();
    while let Some((position, c)) = chars.next() {
        match c {
            // Escapes the next character, including a quote or a line break, in raw
            // strings too (where the backslash is kept but still can't end the string).
            '\\' => {
                if chars.next().is_none() {
                    return false;
                }
            }
            '\n' if quote.len() == 1 => return false,
            _ if content[position..].starts_with(quote) => {
                return position + quote.len() == content.len();
            }
            _ => {}
        }
    }
    false
}
