# harness

Terminal chat client for an OpenAI-compatible chat completions endpoint, with one tool: a Python REPL.

## Files

All paths are relative to the working directory.

- `config.toml`:
  ```toml
  [endpoint]
  base_url = "http://host:8000/v1"   # requests go to {base_url}/chat/completions; trailing `/` trimmed
  model = "your-model-name"
  api_key_env = "OPENAI_API_KEY"     # optional: names the variable holding the key; omitted = no Authorization header

  [chat]                             # optional
  prompt = """..."""                 # chat system message; omitted = none
  tool_reasoning = false             # see Tool reasoning; default false; rewritten by /tool-reasoning

  [classifier]
  prompt = """..."""                 # required: classifier system prompt

  [repl]                             # optional
  approval_mode = "auto"             # "allow" | "ask" | "auto"; default "auto"; rewritten by Shift+Tab
  output_limit = 100000              # bytes of output per call sent to the model; default 100000
  ```
  Unknown keys are rejected. A missing or invalid config, or an `api_key_env` naming an unset variable, is an error before the TUI starts. Shift+Tab rewrites only `approval_mode` (creating `[repl]` if missing), and `/tool-reasoning` only `tool_reasoning` (creating `[chat]` if missing), preserving the rest of the file.
- `.env`: read if present, only to resolve `api_key_env`; shell variables take precedence. It is never added to the process environment, so the REPL doesn't inherit it. No permission checks; the app never edits `.gitignore`.
- `sessions/YYYYMMDD-HHMMSS.json` (local time): `{ "messages": [...] }`. Created on first save. Every save writes a synced temp file and renames it over the session file.
- `replib/*.py`: optional REPL function library (see REPL).

## Messages

- `messages` is exactly the array sent to the endpoint, after the system message. The system message is `[chat] prompt` as currently configured, added at send time and never saved.
  - user: `{"role": "user", "content"}`
  - assistant: `{"role": "assistant", "content", "tool_calls"?, ...}`; `tool_calls` entries are `{"id", "type": "function", "function": {"name", "arguments"}}`. Every other field the endpoint streamed (reasoning under whatever name it uses, and unknown fields, here and inside tool calls) is kept and sent back unchanged.
  - tool: `{"role": "tool", "tool_call_id", "content"}`
- The user message is saved on send. An assistant message is saved once when its response ends (completed, Esc, error, `/exit`, or app exit), with every field that arrived, if any (besides `role`) has a value; `null`, `""`, `[]`, and `{}` don't count. A tool call without an `id` or `function.name` is dropped. A tool call still streaming when the response is interrupted or cut off at the token limit (`finish_reason: "length"`), meaning its arguments aren't valid JSON, is dropped.
- Every tool call in a saved assistant message is followed by exactly one tool message before the next request.
- Messages are only removed by the per-turn `FYI()`/`help()` cleanup (see Tool calling).
- Errors and classifier verdicts are shown in the transcript and never saved.
- `--resume <PATH>` loads the file, shows the whole conversation, and appends to the same file. A missing or unparseable file is an error before the TUI starts. Any tool call without a result gets `[not run: turn stopped]`, inserted after its call's existing results and saved at once, with a line printed to the normal screen before the TUI starts (visible after exit).

## Requests

- Chat: `POST {base_url}/chat/completions` with `{"model", "stream": true, "stream_options": {"include_usage": true}, "tools": [REPL], "messages"}`, plus bearer auth when a key is configured. `messages` starts with `{"role": "system", "content": [chat] prompt}` when configured.
- SSE `data:` lines: each `delta` is merged into the reply by the OpenAI SDK's `accumulate_delta` rule, whatever its fields: strings appended, numbers added, objects merged recursively, lists of entries with an `index` merged by matching `index` value (a fragment without one is appended), other lists extended; `index` and `type` are replaced; a value of a different type (such as a later `null`) is ignored. `role` is always `assistant`; `index` is removed from saved tool calls. `usage.total_tokens` is the context count and `usage.prompt_tokens` is retained for the next turn's FYI snapshot, an `error` object is an error. `[DONE]` ends the stream. EOF without `[DONE]` is an error unless a `finish_reason` was seen. A non-2xx response is an error showing status and body.
- Classifier: non-streaming request to the same endpoint and model. System message: `[classifier] prompt`. User message: `Repository root: <absolute working directory>` and the code in a fenced block. `response_format` is a strict JSON schema `{"effects": string, "verdict": "safe" | "unsafe" | "inconclusive"}`, both required, in that order.

## Tool calling

- The REPL tool definition is fixed in code:
  - name `REPL`, one required string parameter `code`
  - description: "Execute Python in a REPL session. State persists for the lifetime of the current turn — variables and data survive across the tool calls you make during the current turn, but never carry over to a later turn. Call help() to see the available functions and libraries."
- A turn starts with a user message. Each response's tool calls are handled in order, their results are sent, and the next response is requested. The turn ends when a response has no tool calls, on Esc, or on a stream error.
- **Situational awareness.** Each turn opens with synthetic calls. First, every earlier `REPL` call whose code is made only of `FYI()`/`help()` statements (synthetic or the model's own) is removed with its result; an assistant message left with no calls, content, or other field with a value is removed too, and the session is saved. Then, after the user message, the app saves an assistant message with two `REPL` calls, `{"code": "FYI()"}` and `{"code": "help()"}` (ids `fyi-<unix millis>-<counter>`), and runs them in the REPL like any other calls, before the first request. So the context always holds exactly one environment snapshot and one library listing: the current turn's. `FYI()` is the driver's only snapshot implementation; it prints the lines `FYI()`, `Date: <%a %b %d %H:%M:%S %:z %Y>`, `Prompt tokens: <latest usage.prompt_tokens>` (omitted until a response reports usage), and `Model: <model>`, and returns `None`. Honesty rule: a synthetic call is a real call the model could make itself, so an explicit `FYI()` works the same way (its token count is the latest at call time). `FYI()` is not in the registry, so `help()` doesn't list it.
- Synthetic calls run without approval in every mode. Code that is exactly `reasoning(<one string literal>)` (optional `r`/`u` prefix, any quoting; not f-strings, bytes, concatenation, keyword arguments, or other code alongside) is never approved, classified, or run: its result is `(no output)` in every mode, since `reasoning()` is a no-op the model is meant to call freely and the REPL's `reasoning` may have been rebound. Every other call is approved per `approval_mode`.
- Approval, per `approval_mode` at the moment each call is handled:
  - `allow`: run.
  - `ask`: prompt `allow? (y/n)`.
  - `auto`: classify. `safe` runs; `unsafe` returns `Blocked: <effects>`; `inconclusive` or a classifier failure prompts.
  - At the prompt, `y` runs and `n` returns `Denied by user: <effects>` (or `Denied by user.` without effects).
- A call that isn't `REPL`, or whose arguments aren't JSON with a string `code`, returns an error result without running.
- Result: stdout and stderr in written order, then the repr of a trailing expression if not `None`, then a traceback if the code raised; `(no output)` if empty. Truncated at `output_limit` bytes on a character boundary, followed by `[output truncated: N more bytes]`.
- Esc (or exit) while code runs kills the REPL's process group; the result is the output so far plus `[stopped by user]`. Every complete call that never ran gets `[not run: turn stopped]`.
- SIGHUP, SIGTERM, and SIGINT stop the turn the same way (recording only, no transcript output) and exit.

## Tool reasoning

Experimental. With `[chat] tool_reasoning` on, each request and the transcript show the model its reasoning from earlier turns as tool calls instead of native reasoning. The session file is unchanged, so the setting can be switched either way at any time, including for resumed sessions. Deferred extension to the turn in progress: `SPEC-final-turn-tool-reasoning.md`.

- Converted: each assistant message before the last user message whose reasoning text (as displayed; see Terminal UI) is non-empty. Messages with only encrypted `reasoning_details` are left as-is. The turn in progress keeps its native reasoning.
- A converted message is sent as three messages: `{"role": "assistant", "content": "", "tool_calls": [REPL call, id reasoning-<message index>, code reasoning(<text as a JSON string literal>)]}`, `{"role": "tool", "tool_call_id": "reasoning-<message index>", "content": "(no output)"}`, then the message without `reasoning`, `reasoning_content`, and `reasoning_details`. Ids are stable across requests.
- The synthetic calls never run. Their `(no output)` result relies on `replib/reasoning.py` defining `reasoning(text)` as a no-op that prints nothing and returns `None`; `help()` lists it, and the model is meant to call it.

## REPL

- `python3 -u -c <embedded driver>` from `PATH`, in the working directory, as the user, unsandboxed. The driver first calls `setsid()`: it has no controlling terminal (programs opening `/dev/tty` fail immediately), and it leads a process group that is killed as a whole. Started by a turn's first call (the synthetic `FYI()`, so every turn), reused for the turn's later calls, killed when the turn ends. If it dies mid-call, the result ends with `[REPL process exited: …]` and the next call starts a new process.
- Protocol: JSON lines over the process's original stdin/stdout. Host sends `{"code", "prompt_tokens"}` (the latest `usage.prompt_tokens` or `null`, stored for `FYI()`); driver sends `{"output"}` chunks as written, then `{"done": true, "value", "error"}`. The code's stdin is `/dev/null`, and its fds 1 and 2 (inherited by subprocesses) go to a pipe the driver reads.
- Code runs in one namespace per process. A trailing expression's value is returned and bound to `_`. `exit()` doesn't end the REPL.
- `replib/*.py` are executed in sorted order at startup with `register` in scope; `@register` adds a function to the namespace and to `help()`. Load errors are printed in the first call's output.
- `help()` prints the output limit, then each registered function's signature and docstring, or `No registered functions.`. `help(obj)` is Python's `help`.

## Terminal UI

- Full screen on the alternate screen, with mouse capture. On exit the terminal returns to its previous contents; nothing is left behind.
- The transcript always matches what the model sees: every frame draws it from the system message, `messages`, and the turn in progress, so removed or changed messages disappear or change wherever they are. Fields other than content, reasoning, and tool calls are sent but not shown. With tool reasoning on, converted messages are drawn as sent: the synthetic call and its result, then the message without its reasoning.
  - System message (if configured): `system` (dim), then the prompt.
  - User message: `> text`, cyan.
  - Assistant: reasoning dim, a blank line, then content in the default style. Reasoning is the first non-empty of `reasoning`, `reasoning_content`, and the `text` or `summary` of each `reasoning_details` entry (joined by blank lines).
  - Tool call: `REPL` (dim), the code, the verdict note, then the result, dim. A saved reply's calls all appear at once; running output appears live, in full.
  - A blank line follows each message and each tool call.
  - Notes are shown but never saved and are lost on exit: verdicts (`safe:` green, `unsafe:` red, `inconclusive:` or `classifier failed:` yellow; `auto` only) under their call, and errors (`error: ...`, red) where they happened. A verdict goes with its call when the call is removed; errors keep their place among the remaining messages.
- Word-wrapped by the app to the screen width and re-wrapped on resize. Rows break after whitespace; longer words break at the width. Tabs expand to 8-column stops; other control characters are not drawn.
- Scrolling: while at the bottom the view follows new output; scrolled up, it stays on the same text as output arrives or the width changes, until scrolled back to the bottom. Sending a message returns to the bottom.
- Selection: dragging selects transcript text (reverse video); dragging onto the top row or below the transcript scrolls one row per mouse event. Releasing copies the selection's source text (without wrap breaks) with OSC 52. The selection stays until the next click or key press. A failed copy shows the notice `couldn't copy: <error>`.
- Each frame is one synchronized update.
- Bottom region: a dim top rule, a bright white `> ` prompt followed by the input box (word-wrapped, growing up to half the screen height), and a status line, or the command completion list in its place (see Commands); the region grows to fit the list. A click there only clears the selection.
- Status line (dim): `<model> | N tokens | <approval_mode>` (tokens: `total_tokens` of the last response that reported usage; omitted until one has), ` | tool reasoning` while it's on, then ` · responding…`, ` · classifying…`, or ` · running…` `(esc to stop)` while busy, ` · allow? (y/n, esc to stop)` in yellow at a prompt, ` · scrolled up (PgDn)` while scrolled up, and ` · <notice>` in yellow until the next key press.
- Bracketed paste is enabled. Pasted text is inserted as-is (CR and CRLF become LF) and never sends.

## Keys

| Key | Action |
|---|---|
| Enter | Send (ignored when blank); with the completion list open, run the highlighted command |
| ^J | Insert newline |
| ^K | Delete to end of line; at end of line, join the next line |
| ^U | Delete to start of line; at start of line, join the previous line |
| Esc | Stop the turn; with the completion list open, only close it |
| Shift+Tab | Cycle approval mode: ask → auto → allow |
| Up / Down, Tab | With the completion list open: move the highlight (wrapping), complete to the highlighted command |
| y / n | Answer an approval prompt (while it shows, only y, n, Esc, Shift+Tab, PageUp/PageDown, and the mouse act) |
| PageUp / PageDown | Scroll the transcript a screen, less two rows |
| Mouse wheel | Scroll the transcript one row |
| Mouse drag | Select transcript text; copied on release |
| ^C, ^D | Nothing |
| Others | ratatui-textarea defaults |

## Commands

- Input starting with `/` is a command. A leading space sends a literal `/`.
- Completion list: shown below the input in place of the status line while the input is one line starting with `/` without whitespace, some command's name or alias starts with it, and no approval prompt is showing. One row per matching command (by name if it matches, else by its first matching alias) with its description; the first is highlighted, and any edit highlights the first again. At most 8 rows, scrolling with the highlight. Esc closes it until the input changes. Commands are defined once, in `src/commands.rs`, for both running and completion; the list order puts `/exit` last.
- `/exit`, `/quit`: exit.
- `/tool-reasoning`: switch tool reasoning on or off and save it to `config.toml`. The transcript and the next request change at once.
- Unknown command: notice `unknown command: <input>`; the input is kept.
- During a turn, the input stays editable and commands run. Enter on a message shows the notice `turn in progress (esc to stop)` and keeps the draft.

## Source

| File | Role |
|---|---|
| `src/main.rs` | Arguments, config and session loading |
| `src/app.rs` | Event loop, turn state machine, streaming display, saving, status line |
| `src/tui.rs` | Alternate screen, mouse capture, screen layout, synchronized updates, clipboard |
| `src/transcript.rs` | Transcript view: word wrap, scrolling, selection |
| `src/input.rs` | Textarea setup, key map, paste |
| `src/commands.rs` | Command table: names, aliases, descriptions, actions |
| `src/client.rs` | REPL tool definition, streaming request, SSE parsing and delta merging, classifier request |
| `src/repl.rs` | REPL process lifecycle and protocol |
| `src/repl_driver.py` | Embedded Python driver |
| `src/config.rs` | `config.toml` (including `approval_mode` and `tool_reasoning` persistence) and `.env` |
| `src/session.rs` | Message types and session file I/O |
| `src/tool_reasoning.rs` | Tool reasoning conversion and the `reasoning()` approval exemption |
| `replib/reasoning.py` | `reasoning()` library function |
