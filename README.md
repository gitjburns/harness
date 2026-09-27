# harness

A terminal coding agent for OpenAI-compatible chat completions endpoints. The model works through a single tool, a Python REPL, so it can read, write, and edit files, run commands, and combine them in ordinary code.

The app runs full screen and draws the conversation exactly as the model sees it, word-wrapped to your terminal's width. Replies stream as they are generated, including the model's reasoning when the server provides it. When you exit, your terminal returns to what it showed before.

> **The REPL runs Python on your machine, as you, with no sandbox.** Approval modes (below) control what runs without asking you.

## Setup

Requires a Rust toolchain and `python3` on your `PATH`.

Create `config.toml` in the directory you run from, normally the root of the repository you want the agent to work in:

```toml
[endpoint]
base_url = "http://localhost:8000/v1"
model = "your-model-name"
api_key_env = "OPENAI_API_KEY"   # optional; omit for servers that need no key

[chat]                           # optional
prompt = """
...system message sent at the start of every request...
"""
tool_reasoning = false           # see Tool reasoning; /tool-reasoning switches it

[classifier]
prompt = """
...instructions for judging whether code is safe to run; see this repo's config.toml...
"""

[repl]                           # optional
approval_mode = "auto"           # allow | ask | auto
output_limit = 100000            # bytes of output per call returned to the model
```

If the endpoint needs a key, put it in `.env` in the same directory (and keep that file out of version control):

```
OPENAI_API_KEY=sk-...
```

A variable already set in your shell takes precedence over `.env`.

## Running

```
cargo run
```

Each run starts a new conversation, saved to `sessions/YYYYMMDD-HHMMSS.json` when you send the first message. To continue an earlier conversation:

```
cargo run -- --resume sessions/20260925-143012.json
```

The earlier conversation is shown, and new messages are added to the same file.

## Using it

Type at the `> ` prompt and press Enter to send. The reply streams above the prompt: the model's reasoning appears dimmed, followed by the answer.

When the model uses the REPL, you see the code, then (in `auto` mode) the safety verdict, then the output as it runs. The model keeps going, calling the REPL as many times as it needs, until it answers without one. Esc stops it at any point.

The status line under the prompt shows the model, the number of tokens in use (after the first reply), and the approval mode.

While the model is working, you can keep typing your next message. It sends once the turn finishes and you press Enter.

**Situational awareness.** Each turn starts with the app calling `FYI()` and then `help()` in the REPL on the model's behalf. `FYI()` prints the current date, the prompt-token count of the latest reply (once there is one), and the model; `help()` lists the output limit and library functions. The calls and their output are shown and saved like any other, and the model can call either itself at any time. Before adding them, the app removes every earlier call made only of `FYI()`/`help()` from the conversation and its session file, so only the current turn's copies are in context. The app's own `FYI()` and `help()` calls never need approval, in any mode; when the model calls them itself, the call is approved like any other.

### Approval modes

Shift+Tab cycles the mode; the choice is saved to `config.toml`.

| Mode | What happens to each REPL call |
|---|---|
| `ask` | You approve every call with `y` or deny it with `n`. |
| `auto` | A separate request to the model judges the code against `[classifier] prompt`. Safe code runs, unsafe code is blocked, and anything else asks you. |
| `allow` | Every call runs without asking. |

A call that is only `reasoning("...")` on a plain string never needs approval, in any mode (see Library functions).

### Keys

| Key | Action |
|---|---|
| Enter | Send |
| Ctrl+J | New line |
| Ctrl+K | Delete to end of line |
| Ctrl+U | Delete to start of line |
| Esc | Stop the model (with the command list open, close the list) |
| Shift+Tab | Change approval mode |
| y / n | Allow or deny a REPL call when asked |
| PageUp / PageDown | Scroll the conversation a screen |
| Mouse wheel | Scroll the conversation a line |
| Mouse drag | Select text; it's copied when you release |

Pasting multi-line text never sends it; press Enter when ready.

When you scroll up, the view stays put while the model writes; scroll back to the bottom to follow it again. Copying uses the OSC 52 escape sequence, which some terminals don't support or need enabled (in iTerm2, allow clipboard access in settings; in tmux, set `set-clipboard on`). Where it doesn't work, your terminal's own selection still does, usually by holding Option (macOS) or Shift while dragging.

### Commands

| Command | Action |
|---|---|
| `/exit`, `/quit` | Exit |
| `/tool-reasoning` | Turn tool reasoning on or off (saved to `config.toml`) |

Typing `/` opens a list of matching commands below the input: Up/Down to choose, Tab to complete, Enter to run, Esc to close.

To send a message that starts with `/`, begin it with a space.

## The REPL

REPL state (variables, imports) lasts for one turn: from your message until the model's final answer. Each turn starts fresh. Calling `help()` in the REPL lists the output limit and any library functions.

### Library functions

Library functions are Python functions you provide for the model. They live in `replib/`, one or more `.py` files in the directory you run from, loaded in name order at the start of every turn. The model learns about them only from `help()`, which lists each one's signature and docstring.

**Adding one.** Put it in any file in `replib/` and mark it with `@register` (no import needed):

```python
@register
def word_count(path):
    """Count the words in a file."""
    with open(path) as f:
        return len(f.read().split())
```

**Editing a description.** The docstring is exactly what the model reads, so it is the place to say what a function is for and when to use it. Edit it in place; the change takes effect from the next turn, since each turn starts a fresh REPL.

**Removing one.** Delete the function, or the whole file. Code in `replib/` without `@register` still runs at load, but isn't listed or offered to the model.

**Mistakes.** If a file fails to load, its error appears in the output of the turn's first call (the app's own `FYI()`), so you and the model both see it; the other files still load.

This repo ships one library function, `replib/reasoning.py`:

- `reasoning(text)` does nothing and returns `None`. It gives the model a way to record its reasoning as part of the conversation, and the model is encouraged to call it: a call that is only `reasoning("...")` on a plain string never needs approval.
- Tool reasoning (below) depends on it staying a no-op that prints nothing. Edit its docstring freely, but not its behavior.

## Tool reasoning

Experimental. When on, the model sees its reasoning from earlier turns as `reasoning("...")` REPL calls, each followed by its `(no output)` result, instead of as native reasoning. The current turn keeps its native reasoning. Turn it on or off with `/tool-reasoning`; the status line shows `| tool reasoning` while it's on.

Only what's sent and shown changes: the session file keeps replies as the endpoint sent them, so switching it off restores the original view at once, including for resumed conversations. It requires `reasoning()` in `replib/` (above).

## Conversation files

Session files hold the conversation exactly as it is sent to the model, including tool calls and their results. The `[chat] prompt` system message isn't saved; each request uses the one currently in `config.toml`, including for resumed conversations:

```json
{ "messages": [
  { "role": "user", "content": "..." },
  { "role": "assistant", "content": "", "reasoning": "...", "tool_calls": [ ... ] },
  { "role": "tool", "tool_call_id": "...", "content": "..." },
  { "role": "assistant", "content": "...", "reasoning": "..." }
] }
```

Replies keep every field the endpoint sent, such as reasoning under whatever name it uses (`reasoning`, `reasoning_content`, `reasoning_details`, …), and send them back unchanged. A reply you stop partway through is kept as far as it got. Errors and safety verdicts appear on screen but are not saved.
