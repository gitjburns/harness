# harness

A terminal coding agent for OpenAI-compatible chat completions endpoints. The model works through a single tool, a Python REPL, so it can read, write, and edit files, run commands, and combine them in ordinary code.

The app runs full screen and draws the conversation exactly as the model sees it, word-wrapped to your terminal's width. Replies stream as they are generated, including the model's reasoning when the server provides it. When you exit, your terminal returns to what it showed before.

The REPL uses Monty, a sandboxed Python-subset interpreter. Repository filesystem access follows the permission level below. **Commands launched with `run()` are host processes running as you, outside the REPL sandbox.**

## Setup

Requires a Rust toolchain. The Monty interpreter is built into the app.

The app keeps its files in `~/.harness`: `config.toml`, `.env`, `sessions/`, `replib/`, optional `repl-notes.md`, and `failures.jsonl`. They are shared by every repository you run it in. `~/.harness` can be a symlink to a checkout of this repo.

Create `~/.harness/config.toml`:

```toml
[endpoint]
base_url = "http://localhost:8000/v1"
model = "your-model-name"
api_key_env = "OPENAI_API_KEY"   # optional; omit for servers that need no key

[chat]
prompt = """
...optional system message sent at the start of every request...
"""
tool_reasoning = false           # see Tool reasoning; /tool-reasoning switches it

[repl]
tool_description = """
Execute Python in a sandboxed Monty REPL. State lasts for one turn. Call help() for available functions and libraries. Use run() for host commands.
"""
permission = "none"              # none | read-only | read-write
max_memory_mb = 2048             # worker memory limit in MiB
output_limit = 100000            # bytes of output per call returned to the model

[commands]
deny = ["git", "rm"]             # always denied, even if also allowed
env_filter = ["GITHUB_TOKEN"]    # exact names removed from command environments

[commands.allow]
# cargo = "/absolute/path/to/cargo"  # configured commands run without asking
```

These are example values, not runtime defaults. All settings shown are required except `api_key_env`, `chat.prompt`, and individual allow-list entries. Keep `[commands.allow]` even when empty. Missing required settings, unknown keys, and relative allow-list executable paths are startup errors. `tool_description` is sent verbatim to the model. See `config.toml.example` for a complete example.

If the endpoint needs a key, put it in `~/.harness/.env` (and keep that file out of version control):

```
OPENAI_API_KEY=sk-...
```

A variable already set in your shell takes precedence over `.env`. The app reads `.env` privately; it does not export those values to command processes.

## Running

Build with `cargo build`, then run `harness` (in `target/debug/`) from the directory the agent should work in, normally the root of a repository.

Each run starts a new conversation, saved to `~/.harness/sessions/YYYYMMDD-HHMMSS.json` when you send the first message. The file name without `.json` is the session's name; `/rename <name>` changes it. To list saved sessions, oldest first, or continue one:

```
harness --resume
harness --resume 20260925-143012
```

The earlier conversation is shown, and new messages are added to the same file.

Override `[chat] prompt` for this invocation with literal text:

```sh
harness --system-prompt "Your system prompt"
```

This also works with `--resume <NAME>`. The text is used verbatim, including an empty string, without changing configuration or saving the prompt in the session. Omit the option to use the configured prompt.

## Using it

Type at the `> ` prompt and press Enter to send. The reply streams above the prompt: the model's reasoning appears dimmed, followed by the answer.

When the model uses the REPL, you see the code and its printed output as it runs. Host commands return captured output when they finish. The model keeps calling the REPL until it answers without one. Esc stops it at any point.

The status line under the prompt shows the model, the number of tokens in use (after the first reply), and the filesystem permission level. Pending approvals show the operation in yellow; press `y` to allow or `n` to deny.

While the model is working, you can keep typing your next message. It sends once the turn finishes and you press Enter.

**Situational awareness.** Each turn starts with the app calling `FYI()` and then `help()` in the REPL on the model's behalf. `FYI()` prints the current date, the prompt-token count of the latest reply (once there is one), and the model; `help()` lists the output limit and library functions. The calls and their output are shown and saved like any other, and the model can call either itself at any time. Before adding them, the app removes every earlier call made only of `FYI()`/`help()` from the conversation and its session file, so only the current turn's copies are in context.

**Approval exemptions.** All synthetic tool calls, including `FYI()` and `help()` and any added in the future, never require approval, whether the app generates them or the model calls them explicitly. This applies at every permission level. Unrelated code bundled with them, including code evaluated in arguments, follows normal permissions and approval rules.

### Filesystem permissions

Shift+Tab cycles `none → read-only → read-write`; the choice is saved to `config.toml` and applies to the next filesystem operation, including during a call.

| Level | Repository filesystem access |
|---|---|
| `none` | Reads and writes are denied. |
| `read-only` | Reads are allowed; writes are denied. |
| `read-write` | Reads and writes are allowed. |

The repository is the directory from which you launch the app. Access outside it is denied at every level, including traversal and symlink escapes, without an approval prompt. Monty also rejects absolute symlink targets inside the repository; use relative targets. `Path.resolve()` normalizes paths lexically rather than following host symlinks.

Host command approvals are independent of these levels (see Host commands). `reasoning()` is not synthetic: a standalone `reasoning("...")` on a plain string has its own approval exemption at every level. It cannot exempt unrelated code.

### Keys

| Key | Action |
|---|---|
| Enter | Send |
| Ctrl+J | New line |
| Ctrl+K | Delete to end of line |
| Ctrl+U | Delete to start of line |
| Esc | Stop the model (with the command list open, close the list) |
| Shift+Tab | Change filesystem permission |
| y / n | Allow or deny the pending host operation |
| PageUp / PageDown | Scroll the conversation a screen |
| Mouse wheel | Scroll the conversation a line |
| Mouse drag | Select text; it's copied when you release |

Pasting multi-line text never sends it; press Enter when ready.

When you scroll up, the view stays put while the model writes; scroll back to the bottom to follow it again. Copying uses the OSC 52 escape sequence, which some terminals don't support or need enabled (in iTerm2, allow clipboard access in settings; in tmux, set `set-clipboard on`). Where it doesn't work, your terminal's own selection still does, usually by holding Option (macOS) or Shift while dragging.

### Commands

| Command | Action |
|---|---|
| `/rename <name>` | Rename this session (its file in `~/.harness/sessions/`); refused if the name is taken |
| `/tool-reasoning` | Turn tool reasoning on or off (saved to `config.toml`) |
| `/exit`, `/quit` | Exit |

Typing `/` opens a list of matching commands below the input: Up/Down to choose, Tab to complete, Enter to run (for `/rename`, to complete it so you can type the name), Esc to close.

To send a message that starts with `/`, begin it with a space.

## The REPL

REPL state lasts for one turn: from your message until the model's final answer. Each turn starts fresh. Static type checking is disabled; after an ordinary runtime exception, completed effects and surviving state remain available. A worker crash or memory-limit failure discards that state; the next call starts fresh and reloads the library, with the state loss reported explicitly.

Printed output is followed by the trailing expression's value, when not `None`, and any runtime error. Empty output becomes `(no output)`. Results are capped at `output_limit` bytes on a Unicode character boundary, with a notice stating how many bytes were omitted. The sandbox sees an empty environment.

`help()` lists the output limit, optional guidance from `~/.harness/repl-notes.md`, host functions, loaded public library functions, and allow-listed command names.

### Host commands

`run(argv, cwd=None)` executes a bare command name with literal arguments, without a shell. It returns `{"exit_code": int, "stdout": str, "stderr": str}` and prints nothing. Output is captured until the command finishes; a signal exit uses the negative signal number. Commands have no terminal or interactive stdin. Esc kills the command's process group.

Names in `[commands] deny` are blocked. Names in `[commands.allow]` run the configured absolute executable without asking. Other names are resolved through the app's `PATH` and require approval each time; missing commands produce an error. This is independent of the filesystem permission level.

Commands start in the repository root, independently of sandbox `os.chdir()`. An explicit `cwd` must be relative and contain no `..` components. If its symlinks resolve outside the repository, using that directory requires approval. Commands inherit the app's environment minus the exact names in `env_filter`.

### Library functions

Library functions are Monty-compatible Python functions you provide in `~/.harness/replib/*.py`. Files load in name order at the start of every turn. `help()` lists top-level functions whose names do not start with `_`, using their source signatures and docstrings. `lib_source(name)` returns the current contents of `replib/<name>.py`.

**Adding one.** Define a public function in a `.py` file in `replib/`:

```python
from pathlib import Path as _Path

def word_count(path: str) -> int:
    """Count the words in a file."""
    return len(_Path(path).read_text().split())
```

**Editing a description.** The docstring is exactly what the model reads, so it is the place to say what a function is for and when to use it. Edit it in place; the change takes effect from the next turn, since each turn starts a fresh REPL.

**Removing one.** Delete the function or its file. Top-level code executes during loading; private functions remain callable but are omitted from `help()`.

**Mistakes.** Load errors appear in the turn's first call (the app's `FYI()`); other files still load. Failed files are omitted from `help()`. Library code follows the same permissions as other REPL code.

This repo ships `replib/reasoning.py`, `replib/glob.py`, and `replib/search.py`. Use them through the `~/.harness` symlink or copy them into `~/.harness/replib/`:

- `reasoning(text)` does nothing and returns `None`. It gives the model a way to record its reasoning as part of the conversation, and the model is encouraged to call it: a call that is only `reasoning("...")` on a plain string never needs approval, and the app answers it with `(no output)` without running it.
- That answer, and tool reasoning (below), depend on it staying a no-op that prints nothing. Edit its docstring freely (it's what `help()` shows the model), but not its behavior.
- `glob(pattern, path=None, exclude=(".git", "target", "node_modules"))` returns sorted relative paths.
- `search(pattern, path=None, glob=None, exclude=(".git", "target", "node_modules"))` finds regex matches and returns `path`, 1-based `line`, and `text` records.

Both file helpers default to `.`, preserve the supplied relative root in results, skip directory symlinks, and reject absolute paths and `..` components in path inputs. Errors propagate rather than silently skipping unreadable files.

### Failure notes

App denials and unsupported Monty operations are appended to `~/.harness/failures.jsonl` with time, session, kind, message, and code. A yellow note under the call shows the failure's first line and occurrence count across sessions. These notes are not sent to the model or saved in conversation files; the tool's error result is.

## Tool reasoning

Experimental. When on, the model sees its reasoning from earlier turns as `reasoning("...")` REPL calls, each followed by its `(no output)` result, instead of as native reasoning. The current turn keeps its native reasoning. Turn it on or off with `/tool-reasoning`; the status line shows `| tool reasoning` while it's on.

Only what's sent and shown changes: the session file keeps replies as the endpoint sent them, so switching it off restores the original view at once, including for resumed conversations. It requires `reasoning()` in `replib/` (above).

## Conversation files

Session files hold the conversation exactly as it is sent to the model, including tool calls and their results. The system message isn't saved; each request uses this invocation's `--system-prompt` override or the prompt loaded from `config.toml`, including for resumed conversations:

```json
{ "messages": [
  { "role": "user", "content": "..." },
  { "role": "assistant", "content": "", "reasoning": "...", "tool_calls": [ ... ] },
  { "role": "tool", "tool_call_id": "...", "content": "..." },
  { "role": "assistant", "content": "...", "reasoning": "..." }
] }
```

Replies keep every field the endpoint sent, such as reasoning under whatever name it uses (`reasoning`, `reasoning_content`, `reasoning_details`, …), and send them back unchanged. A reply you stop partway through is kept as far as it got. Tool results include their errors; display-only notices and failure-count notes are not saved in the conversation.
