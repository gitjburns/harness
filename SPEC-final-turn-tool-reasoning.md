# Final-turn tool reasoning (deferred)

Status: designed, not implemented. Extends the tool-reasoning toggle from every turn except the one in progress to the turn in progress.

## Why

Models attend to reasoning tokens differently than to tool calls and other response tokens. Showing the model its reasoning as a tool call gives it a different view of that reasoning, not a duplicate of it. The base feature does this for earlier turns; this extension does it within the turn in progress, while the reasoning is still being acted on.

## Base feature this builds on

These points were decided for the base feature and apply here unchanged:

- **Converted form.** A reply with reasoning becomes, in order: a synthetic assistant message whose only content is a `REPL` call with code `reasoning("<reasoning text>")`, that call's tool result `(no output)`, then the reply itself without its native reasoning fields. Reasoning comes before the answer and before any real calls, in the order the model produced them.
- **`reasoning()`** is a real driver function that does nothing and returns `None`, so `(no output)` is exactly what the call produces. Honesty rule: the model gets the same result if it calls `reasoning()` itself.
- **Conversion happens when each request is built.** The session file keeps replies exactly as streamed, native reasoning fields included. Requests and the transcript are built from the session with conversion applied, so the toggle can be switched either way at any time, including on resumed sessions. Synthetic ids must be stable across requests, so they're derived from the session (for example the message's position), not random.
- **Scope.** Every turn except the one in progress. The turn in progress keeps its native reasoning.

## The extension

Within the turn in progress, each response is one of two kinds:

1. **Reasoning allowed:** a response to the user's message or to a real tool result. The app streams it until the reasoning ends, meaning the first delta with non-empty `content` or `tool_calls`, and then **closes the connection**. The reply is saved with its reasoning only; the content or tool-call fragment that triggered the cut is discarded. The next request is sent immediately.
2. **Reasoning disabled:** a response to a synthetic `reasoning()` result. The request carries the converted reasoning and has reasoning turned off, and the response completes normally with an answer or tool calls. Real tool calls run, and the response to their results is again kind 1.

The result: every response is either cut when its reasoning ends or has no reasoning, so all of the turn's reasoning is converted and none is left native.

**How reasoning is disabled.** There's no standard parameter for it. vLLM and llama.cpp take `chat_template_kwargs` (the key depends on the model's template, for example `enable_thinking`), OpenRouter takes `reasoning: {"enabled": false}`, and OpenAI takes `reasoning_effort`. So config supplies request-body fields that the app merges into kind-2 requests as-is, without interpreting them; the app stays endpoint-neutral. The extension is active only when the setting is present. Example:

```toml
[chat]
reasoning_off = { chat_template_kwargs = { enable_thinking = false } }
```

Switching reasoning on and off between requests doesn't confuse the model: each request is rendered from scratch, and the model sees only the template's normal no-thinking form.

**What's sent for a stopped reply.** Only the converted form. The stopped reply has no content or calls, so its native form would be an assistant message holding nothing but reasoning. If an endpoint turns out to require native reasoning mid-turn (signed reasoning, for example), sending both forms is the fallback.

## Example turn

Synthetic `FYI()`/`help()` calls are left out.

1. Send `{"role": "user", "content": "What files are here?"}`. The response streams `"reasoning": "I should list the directory."`, then a tool call begins → **close the connection.** Save `{"role": "assistant", "content": "", "reasoning": "I should list the directory."}`.
2. Send, **with reasoning disabled**:
   ```json
   { "role": "user", "content": "What files are here?" },
   { "role": "assistant", "content": "", "tool_calls": [ REPL: reasoning("I should list the directory.") ] },
   { "role": "tool", "content": "(no output)" }
   ```
   The response completes normally, for example with a call to `os.listdir()`. The call runs.
3. The response to that result streams reasoning → **close the connection** when it ends, as in step 1. Then continue as in step 2, until a kind-2 response has no tool calls, which ends the turn.

## Edge cases to settle when implementing

- A kind-1 response that ends (`[DONE]`, or `finish_reason: "length"`) during or right after its reasoning, without any content or calls: treat it like a cut.
- A kind-1 response with no reasoning at all: nothing to cut, so it completes normally.
- The endpoint ignores the disable fields and a kind-2 response reasons anyway: it isn't cut (cutting it would loop), so its reasoning stays native. Show a notice.
- Esc during either kind stops the turn as usual; a stopped reply is saved as far as it got.

## Rejected

- **Continuing the stopped message.** The standard protocol can't continue a partial assistant message; a new request produces a new reply.
- **Letting kind-2 responses reason.** Cutting them too never ends. Letting them complete leaves native reasoning that wouldn't otherwise have been generated, mixed with converted reasoning.
- **Adding a converted copy for the turn in progress without cutting** (both forms for replies that made tool calls). Considered, then superseded by this design.

## Cost

Each cut discards the fragment that triggered it and adds a request, whose prompt is processed again (cheap on servers that cache prompt prefixes).

## Open

- Answer quality with reasoning disabled right after converted reasoning. Test live.
- The setting's final name and shape.
