# harness

A terminal coding agent for OpenAI-compatible chat completions endpoints. The model works through a single tool, a Python REPL, so it can read, write, and edit files, run commands, and combine them in ordinary code.

Conversations print into your terminal's normal scrollback, so scrolling, selecting, copying, and resizing work as they do everywhere else in your terminal. Replies stream as they are generated, including the model's reasoning when the server provides it.

> **The REPL runs Python on your machine, as you, with no sandbox.** Approval modes (below) control what runs without asking you.

## Setup

Requires a Rust toolchain and `python3` on your `PATH`.

Create `config.toml` in the directory you run from, normally the root of the repository you want the agent to work in:

```toml
[endpoint]
base_url = "http://localhost:8000/v1"
model = "your-model-name"
api_key_env = "OPENAI_API_KEY"   # optional; omit for servers that need no key

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

The earlier conversation is printed, and new messages are added to the same file.

## Using it

Type at the `> ` prompt and press Enter to send. The reply streams above the prompt: the model's reasoning appears dimmed, followed by the answer.

When the model uses the REPL, you see the code, then (in `auto` mode) the safety verdict, then the output as it runs. The model keeps going, calling the REPL as many times as it needs, until it answers without one. Esc stops it at any point.

The status line under the prompt shows the model, the number of tokens in use (after the first reply), and the approval mode.

While the model is working, you can keep typing your next message. It sends once the turn finishes and you press Enter.

**Situational awareness.** Each turn starts with the app calling `FYI()` and then `help()` in the REPL on the model's behalf. `FYI()` prints the current date, the prompt-token count of the latest reply (`n/a` before the first), and the model; `help()` lists the output limit and library functions. The calls and their output are shown and saved like any other, and the model can call either itself at any time. Before adding them, the app removes every earlier call made only of `FYI()`/`help()` from the conversation and its session file, so only the current turn's copies are in context. Code made only of `FYI()` and `help()` never needs approval, in any mode.

### Approval modes

Shift+Tab cycles the mode; the choice is saved to `config.toml`.

| Mode | What happens to each REPL call |
|---|---|
| `ask` | You approve every call with `y` or deny it with `n`. |
| `auto` | A separate request to the model judges the code against `[classifier] prompt`. Safe code runs, unsafe code is blocked, and anything else asks you. |
| `allow` | Every call runs without asking. |

### Keys

| Key | Action |
|---|---|
| Enter | Send |
| Ctrl+J | New line |
| Ctrl+K | Delete to end of line |
| Ctrl+U | Delete to start of line |
| Esc | Stop the model |
| Shift+Tab | Change approval mode |
| y / n | Allow or deny a REPL call when asked |

Pasting multi-line text never sends it; press Enter when ready.

### Commands

| Command | Action |
|---|---|
| `/exit`, `/quit` | Exit |

To send a message that starts with `/`, begin it with a space.

## The REPL

REPL state (variables, imports) lasts for one turn: from your message until the model's final answer. Each turn starts fresh. Calling `help()` in the REPL lists the output limit and any library functions.

Library functions live in `replib/`, one or more Python files in the directory you run from. Mark a function with `@register` (no import needed) to make it available to the model and listed by `help()`:

```python
@register
def word_count(path):
    """Count the words in a file."""
    with open(path) as f:
        return len(f.read().split())
```

## Conversation files

Session files hold the conversation exactly as it is sent to the model, including tool calls and their results:

```json
{ "messages": [
  { "role": "user", "content": "..." },
  { "role": "assistant", "content": "", "reasoning": "...", "tool_calls": [ ... ] },
  { "role": "tool", "tool_call_id": "...", "content": "..." },
  { "role": "assistant", "content": "...", "reasoning": "..." }
] }
```

A reply you stop partway through is kept as far as it got. Errors and safety verdicts appear on screen but are not saved.
