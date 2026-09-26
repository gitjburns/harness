# harness

Terminal chat client for an OpenAI-compatible chat completions endpoint, with one tool: a Python REPL.

## Files

All paths are relative to the working directory.

- `config.toml`:
  ```toml
  [endpoint]
  base_url = "http://host:8000/v1"   # requests go to {base_url}/chat/completions; trailing `/` trimmed
  model = "DeepSeek-V4-Flash-0731"
  api_key_env = "OPENAI_API_KEY"     # optional: names the variable holding the key; omitted = no Authorization header

  [classifier]
  prompt = """..."""                 # required: classifier system prompt

  [repl]                             # optional
  approval_mode = "auto"             # "allow" | "ask" | "auto"; default "auto"; rewritten by Shift+Tab
  output_limit = 100000              # bytes of output per call sent to the model; default 100000
  ```
  Unknown keys are rejected. A missing or invalid config, or an `api_key_env` naming an unset variable, is an error before the TUI starts. Shift+Tab rewrites only `approval_mode` (creating `[repl]` if missing), preserving the rest of the file.
- `.env`: read if present, only to resolve `api_key_env`; shell variables take precedence. It is never added to the process environment, so the REPL doesn't inherit it. No permission checks; the app never edits `.gitignore`.
- `sessions/YYYYMMDD-HHMMSS.json` (local time): `{ "messages": [...] }`. Created on first save. Every save writes a synced temp file and renames it over the session file.
- `replib/*.py`: optional REPL function library (see REPL).

## Messages

- `messages` is exactly the array sent to the endpoint. No system prompt.
  - user: `{"role": "user", "content"}`
  - assistant: `{"role": "assistant", "content", "reasoning"?, "tool_calls"?}`; `tool_calls` entries are `{"id", "type": "function", "function": {"name", "arguments"}}`
  - tool: `{"role": "tool", "tool_call_id", "content"}`
- The user message is saved on send. An assistant message is saved once when its response ends (completed, Esc, error, `/exit`, or app exit), with whatever content, reasoning, and complete tool calls arrived, if any is non-empty. A tool call still streaming when the response is interrupted or cut off at the token limit (`finish_reason: "length"`), meaning its arguments aren't valid JSON, is dropped.
- Every tool call in a saved assistant message is followed by exactly one tool message before the next request.
- Messages are only removed by the per-turn `FYI()`/`help()` cleanup (see Tool calling).
- Errors and classifier verdicts are shown in the transcript and never saved.
- `--resume <PATH>` loads the file, prints the whole conversation, and appends to the same file. A missing or unparseable file is an error before the TUI starts. Any tool call without a result gets `[not run: turn stopped]`, inserted after its call's existing results and saved at once, with a line printed before the TUI starts.

## Requests

- Chat: `POST {base_url}/chat/completions` with `{"model", "stream": true, "stream_options": {"include_usage": true}, "tools": [REPL], "messages"}`, plus bearer auth when a key is configured.
- SSE `data:` lines: `delta.reasoning` is reasoning, `delta.content` is content, `delta.tool_calls` are tool call fragments (by `index`; `id` and `function.name` once, `function.arguments` appended), `usage.total_tokens` is the context count and `usage.prompt_tokens` is retained for the next turn's FYI snapshot, an `error` object is an error. `[DONE]` ends the stream. EOF without `[DONE]` is an error unless a `finish_reason` was seen. A non-2xx response is an error showing status and body.
- Classifier: non-streaming request to the same endpoint and model. System message: `[classifier] prompt`. User message: `Repository root: <absolute working directory>` and the code in a fenced block. `response_format` is a strict JSON schema `{"effects": string, "verdict": "safe" | "unsafe" | "inconclusive"}`, both required, in that order.

## Tool calling

- The REPL tool definition is fixed in code:
  - name `REPL`, one required string parameter `code`
  - description: "Execute Python in a REPL session. State persists for the lifetime of the current turn — variables and data survive across the tool calls you make during the current turn, but never carry over to a later turn. Call help() to see the available functions and libraries."
- A turn starts with a user message. Each response's tool calls are handled in order, their results are sent, and the next response is requested. The turn ends when a response has no tool calls, on Esc, or on a stream error.
- **Situational awareness.** Each turn opens with synthetic calls. First, every earlier `REPL` call whose code is made only of `FYI()`/`help()` statements (synthetic or the model's own) is removed with its result; an assistant message left with no calls, content, or reasoning is removed too, and the session is saved. Then, after the user message, the app saves an assistant message with two `REPL` calls, `{"code": "FYI()"}` and `{"code": "help()"}` (ids `fyi-<unix millis>-<counter>`), and runs them in the REPL like any other calls, before the first request. So the context always holds exactly one environment snapshot and one library listing: the current turn's. `FYI()` is the driver's only snapshot implementation; it prints `Date: <%a %b %d %H:%M:%S %:z %Y>`, `Prompt tokens: <latest usage.prompt_tokens, or n/a>`, and `Model: <model>`, and returns `None`. Honesty rule: a synthetic call is a real call the model could make itself, so an explicit `FYI()` works the same way (its token count is the latest at call time). `FYI()` is not in the registry, so `help()` doesn't list it.
- Code made only of `FYI()` and `help()` statements (separated by `;` or newlines, whitespace ignored) runs without approval in every mode. Any other code, including code that also calls them or `help(x)`, is approved per `approval_mode`.
- Approval, per `approval_mode` at the moment each call is handled:
  - `allow`: run.
  - `ask`: prompt `allow? (y/n)`.
  - `auto`: classify. `safe` runs; `unsafe` returns `Blocked: <effects>`; `inconclusive` or a classifier failure prompts.
  - At the prompt, `y` runs and `n` returns `Denied by user: <effects>` (or `Denied by user.` without effects).
- A call that isn't `REPL`, or whose arguments aren't JSON with a string `code`, returns an error result without running.
- Result: stdout and stderr in written order, then the repr of a trailing expression if not `None`, then a traceback if the code raised; `(no output)` if empty. Truncated at `output_limit` bytes on a character boundary, followed by `[output truncated: N more bytes]`.
- Esc (or exit) while code runs kills the REPL's process group; the result is the output so far plus `[stopped by user]`. Every complete call that never ran gets `[not run: turn stopped]`.
- SIGHUP, SIGTERM, and SIGINT stop the turn the same way (recording only, no transcript output) and exit.

## REPL

- `python3 -u -c <embedded driver>` from `PATH`, in the working directory, as the user, unsandboxed. The driver first calls `setsid()`: it has no controlling terminal (programs opening `/dev/tty` fail immediately), and it leads a process group that is killed as a whole. Started by a turn's first call (the synthetic `FYI()`, so every turn), reused for the turn's later calls, killed when the turn ends. If it dies mid-call, the result ends with `[REPL process exited: …]` and the next call starts a new process.
- Protocol: JSON lines over the process's original stdin/stdout. Host sends `{"code", "prompt_tokens"}` (the latest `usage.prompt_tokens` or `null`, stored for `FYI()`); driver sends `{"output"}` chunks as written, then `{"done": true, "value", "error"}`. The code's stdin is `/dev/null`, and its fds 1 and 2 (inherited by subprocesses) go to a pipe the driver reads.
- Code runs in one namespace per process. A trailing expression's value is returned and bound to `_`. `exit()` doesn't end the REPL.
- `replib/*.py` are executed in sorted order at startup with `register` in scope; `@register` adds a function to the namespace and to `help()`. Load errors are printed in the first call's output.
- `help()` prints the output limit, then each registered function's signature and docstring, or `No registered functions.`. `help(obj)` is Python's `help`.

## Terminal UI

- No alternate screen and no mouse capture. The terminal owns scrolling, selection, copy, and reflow.
- The transcript is printed raw into scrollback and never hard-wrapped by the app. Control characters other than tab are stripped.
  - User message: `> text`, cyan.
  - Assistant: reasoning dim, a blank line, then content in the default style.
  - Tool call: `REPL` (dim), the code, the verdict line (`safe:` green, `unsafe:` red, `inconclusive:` or `classifier failed:` yellow; `auto` only), then the output, dim, streamed live and in full.
  - A blank line follows each message and each tool call. Errors print red as `error: ...`.
  - On resume, tool results print under their calls; verdict lines aren't shown. The whole replay is written in one write with one cursor query, so it appears at once: the end fills the screen and the rest is in scrollback (up to the terminal's scrollback limit).
- Streaming: the unterminated last line is reprinted from its start on each update. A line taller than half the screen is committed with a hard break.
- Each frame (transcript output plus region redraw) is one synchronized update.
- Bottom region: a dim top rule, a bright white `> ` prompt followed by the input box (word-wrapped, growing up to half the screen height), and a status line.
- Status line (dim): `<model> | N tokens | <approval_mode>` (tokens: `total_tokens` of the last response that reported usage), then ` · responding…`, ` · classifying…`, or ` · running…` `(esc to stop)` while busy, ` · allow? (y/n, esc to stop)` in yellow at a prompt, and ` · <notice>` in yellow until the next key press.
- The hardware cursor is hidden and parked at the region's top-left cell. After a resize, its row is the region's new top and the region is redrawn.
- Bracketed paste is enabled. Pasted text is inserted as-is (CR and CRLF become LF) and never sends.

## Keys

| Key | Action |
|---|---|
| Enter | Send (ignored when blank) |
| ^J | Insert newline |
| ^K | Delete to end of line; at end of line, join the next line |
| ^U | Delete to start of line; at start of line, join the previous line |
| Esc | Stop the turn |
| Shift+Tab | Cycle approval mode: ask → auto → allow |
| y / n | Answer an approval prompt (other keys are ignored while it shows) |
| ^C, ^D | Nothing |
| Others | ratatui-textarea defaults |

## Commands

- Input starting with `/` is a command. A leading space sends a literal `/`.
- `/exit`, `/quit`: exit.
- Unknown command: notice `unknown command: <input>`; the input is kept.
- During a turn, the input stays editable and commands run. Enter on a message shows the notice `turn in progress (esc to stop)` and keeps the draft.

## Source

| File | Role |
|---|---|
| `src/main.rs` | Arguments, config and session loading |
| `src/app.rs` | Event loop, turn state machine, streaming display, saving, status line |
| `src/tui.rs` | Scrollback printing, input region, resize, synchronized updates |
| `src/input.rs` | Textarea setup, key map, paste |
| `src/client.rs` | REPL tool definition, streaming request and SSE parsing, classifier request |
| `src/repl.rs` | REPL process lifecycle and protocol |
| `src/repl_driver.py` | Embedded Python driver |
| `src/config.rs` | `config.toml` (including `approval_mode` persistence) and `.env` |
| `src/session.rs` | Message types and session file I/O |
