# Implementation Plan

Working log. Behavior contract: `SPEC.md`.

## Done

1. **Terminal layer spike.** Raw transcript in scrollback plus an app-managed bottom input region; resize via the parked hidden cursor; bracketed paste.
2. **Chat app.** `config.toml` and `.env`, session files with atomic writes, SSE streaming client, `/exit` and `/quit`, Esc to stop, `--resume <PATH>`.
3. **Review fixes.** Status line erased at screen bottom; partial reply lost on error paths; raw mode left on failed startup; stale pending-line row after scroll; per-delta cursor-query lag (deltas coalesced, input poll 20 ms); resize ordering (`biased` select); temp file fsync.
4. **Flicker fix.** Synchronized updates per frame.
5. **Reasoning.** Streamed as `delta.reasoning`, displayed dim, saved as `reasoning`, sent back.
6. **Input prompt.** Bright white `> ` in the input box.
7. **Context tokens.** `stream_options.include_usage`; status line shows `total_tokens`.
8. **Tool calling.** Single `REPL` tool; per-turn local Python driver (`src/repl_driver.py`) with `replib/` registry and `help()`; `approval_mode` allow/ask/auto (Shift+Tab, persisted via `toml_edit`); LLM classifier for `auto`; `output_limit` (default 100 KB).
9. **Tool-calling hardening (review).** Results recorded before terminal I/O; driver `setsid()` (no controlling tty); request-reader thread; driver survives closed stdout; config save keeps decor, handles inline tables, atomic; `finish_reason: "length"` drops cut-off calls; SIGHUP/SIGTERM/SIGINT cleanup; resume answers unanswered calls; `.env` read privately.
10. **Situational awareness.** Each turn opens with synthetic `FYI()` and `help()` REPL calls, run in the REPL like any other (honesty rule: synthetic calls must work when made explicitly). `FYI()` is implemented only in the driver: date, latest `prompt_tokens` (sent with every code request), model.
11. **Approval exemption.** Code made only of `FYI()` and `help()` statements never needs approval.
12. **Instant resume replay.** The session replays as one batched write, so it appears at once instead of visibly scrolling.
13. **Synthetic-call cleanup.** Each turn removes all earlier `FYI()`/`help()`-only calls (and results, and emptied assistant messages) from the session before adding its own, so exactly one of each is in context. First case of the app removing messages.

Decided against:
- App-side word wrap of the transcript (it would break terminal reflow and copy).
- Audit-hook gating as the first permission layer (classifier chosen instead).
- A driver → app callback for `FYI()` (Python must not call Rust).

## Next

1. **Sandbox.** OS-enforced containment of the REPL: Seatbelt (`sandbox-exec`) on macOS, Landlock (+ seccomp) or bubblewrap on Linux. Writes limited to the repo, no network; privileged operations via app-brokered functions that ask the user.
2. **Context management.** Proactive pruning so compaction is never needed; replaces the interim `output_limit`.
3. **`replib/` functions.** File helpers (`read`/`write`/`edit`/`run`), subagents, knowledge-base search. Subagent-style functions need a driver → app callback in the protocol.
4. **Audit hooks** (optional, inside the sandbox): prompt on ordinary Python calls instead of failing.

## Open

- Two instances started in the same second share a session file name and overwrite each other.
- Process-group kill uses `/bin/kill` to avoid a direct `libc` dependency.

## Reference

- Run: `cargo run` from the repo root; `cargo run -- --resume sessions/<file>.json`.
- Endpoint: vLLM 0.30.0 at `http://10.1.0.10:8000/v1`, model `DeepSeek-V4-Flash-0731`, `max_model_len` 550000, no API key.
- The model's chat template renders assistant `reasoning` only after the last user message (checked with `/tokenize`). `reasoning_content` is ignored on input.
- crossterm's `EventStream` can't be used: it holds the input lock while waiting, which blocks `cursor::position()`.
- The endpoint accepts tool calls with truncated (invalid JSON) `arguments`; other endpoints may not, hence dropping them.
- Python audit hooks (PEP 578) report `open`, `os.remove`/`rename`, `subprocess.Popen`, `os.system`, `socket.connect`, `ctypes.*`, and `import` events with real arguments; not a security boundary.
