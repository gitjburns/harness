# Sandboxed REPL

Replaces the CPython REPL with Monty (`monty-pool` 1.0.0), a Python-subset interpreter with no filesystem, environment, network, or process access of its own. Everything outside the sandbox goes through the app.

Every error, denial, and failure the app reports to the model states what happened, why, and any consequence for the session (such as lost state). Messages below are the standard.

## Config

Removed: `[classifier]`, `[repl] approval_mode`. Added:

```toml
[repl]
tool_description = "..."     # REPL tool description, sent verbatim
permission = "none"          # "none" | "read-only" | "read-write"; rewritten by Shift+Tab
max_memory_mb = 2048         # REPL worker memory limit in MiB

[commands]
deny = ["git", "rm"]         # command names; always denied
env_filter = ["GITHUB_TOKEN"] # exact variable names removed from commands' environment

[commands.allow]             # name the model calls = absolute path that runs
cargo = "/Users/me/.cargo/bin/cargo"
```

`tool_description`, `permission`, and `max_memory_mb` are required; omission is a startup error. `tool_description` is loaded at startup and used verbatim in every REPL tool definition. The values above are examples, not runtime defaults. An `allow` value that isn't an absolute path is a config error. A name in both `deny` and `allow` is denied.

## REPL

- Monty worker: the harness binary run as a hidden worker subcommand (`monty_proto::worker::Child` over stdio, `monty_alloc::LimitedAllocator` as global allocator), launched via `current_exe()` by a `monty_pool::Pool` with one idle worker and at most one worker.
- A turn checks out a worker (fresh session), feeds `replib/*.py`, then runs the turn's calls; state persists across the turn's calls. The turn's end finishes the session and returns the worker. Esc or exit drops the checkout, discarding the worker. A crashed worker ends the call with `[REPL worker crashed: <reason>. The REPL was restarted: every variable, import, and function defined earlier in this turn is gone; library functions are reloaded.]`; the next call gets that new session.
- Static type checking is disabled. Type annotations are not enforced; runtime errors stop execution where they occur. Completed effects and surviving REPL state remain available to later calls after ordinary exceptions. Host functions validate arguments and enforce permissions at each operation.
- Limits: `max_suspensions = usize::MAX`; `max_memory` from `[repl] max_memory_mb`; no time limits (Esc stops). Hitting the memory limit discards the worker and ends the call with `[REPL memory limit of <N> MiB exceeded. The REPL was restarted: every variable, import, and function defined earlier in this turn is gone; library functions are reloaded.]`.
- The repo (working directory) is mounted read-write at its real absolute path, which is also the sandbox working directory. The directory handle is opened once and reused across feeds and turns. `os.getenv`/`os.environ` see an empty environment.
- Result: `print` output in order, then the repr of a trailing expression if not `None`, then a traceback if the code raised; `(no output)` if empty; truncated at `output_limit` as before.
- The approval exemption contract in `SPEC.md` is preserved: all synthetic tool calls, present and future, including `FYI()` and `help()`, never require approval at any permission level, whether app-generated or explicitly called by the model. Unrelated code bundled with them, including code evaluated in arguments, remains subject to normal permission and approval rules.
- `reasoning()` is not synthetic and remains separately approval-exempt at every permission level. Exact `reasoning(<string literal>)` calls are answered `(no output)` without running; unrelated code cannot inherit the exemption.

## Permissions

Every file operation reaches the app as an OS call and is checked before it is serviced:

- Inside the repo: `none` denies all; `read-only` denies writes; `read-write` allows all.
- Filesystem operations are serviced exclusively by the repository mount. Outside access is denied at every level, with no outside-path approvals or host filesystem handlers.
- Unmodified Monty enforces containment against traversal and symlink escapes. Absolute paths and `..` components are not rejected solely for their spelling when the mount contains the operation. Monty rejects absolute symlink targets even inside the repo; filesystem predicates can return `False` for rejected paths. `Path.resolve()` uses Monty's lexical normalization, not host symlink resolution.
- A change of level applies from the next operation, including mid-call.
- App denials raise `PermissionError` at the call, e.g. `write /repo/x: denied: the permission level is read-only`, `read /etc/hosts: denied: the path is outside the repository`, `run 'git': denied: 'git' is on the command deny list`.

## Commands

`run(argv, cwd=None)` → `{"exit_code": int, "stdout": str, "stderr": str}`; prints nothing. A command killed by a signal has `exit_code` = minus the signal number.

- `argv[0]` is a bare command name; one containing `/` raises `ValueError: run takes a command name, not a path: './build.sh'`.
- In `deny`: denied. In `allow`: runs the configured path without asking. Otherwise the app resolves the name through its own `PATH` and asks `allow <path> <args>?`; not found raises `FileNotFoundError: run: no command named 'npm' is configured or found on the host`. Independent of the permission level.
- `cwd` defaults to the repo root. An explicit `cwd` must be relative with no `..` components and resolves against the repo root, independently of sandbox `os.chdir()`; if symlink resolution places the directory outside the repo, it asks.
- The program runs directly (no shell), stdin `/dev/null`, in its own session with no controlling terminal, with the app's environment minus `env_filter`. Commands are host processes, not confined by the REPL mount. Esc kills the process group. No timeout. Output is decoded as UTF-8 with replacement and never shown live.

## Library

- `~/.harness/replib/*.py` is Monty code, fed in name order at session start. Load errors are printed in the first call's output. The repo ships `reasoning.py`, `glob.py`, and `search.py`.
- Public library functions are the top-level functions whose names don't start with `_`. Monty functions expose no name, signature, or docstring, so the app reads these from the source with `ruff_python_parser`.
- `lib_source(name)`: returns the contents of `replib/<name>.py`.
- `glob` and `search` return relative string paths preserving the supplied root (default `.`), skip directory symlinks, and reject absolute paths and `..` components in their path inputs. `search` returns `{"path", "line", "text"}` records with 1-based line numbers; its regex is not a path input.
- Model-authored library changes are deferred: `lib_define`, `lib_remove`, scratch validation, `lib_auto_allow`, and `/lib-auto` are outside this implementation.

## Host functions

- `FYI()`: unchanged output.
- `help()`: the output limit, then `~/.harness/repl-notes.md` if present, then host functions, public library functions (signature and docstring; files that failed to load this session are omitted), and the `allow` command names.
- `run`, `lib_source`: above.

## Failures

- App denials and `NotImplementedError` from Monty are appended to `~/.harness/failures.jsonl`: `{"time", "session", "kind", "message", "code"}`, `kind` one of `denied`, `unsupported`. Existing `type check` records remain readable.
- Counts per `(kind, first line of message)` are loaded at startup and kept in memory.
- Each failure shows a yellow note under its call: `<kind>: <first line> (seen N times)`, N including this one. Notes are never saved or sent.

## Terminal UI

- Status line: `<model> | N tokens | <permission>`, ` | tool reasoning` while on; activity as before without ` · classifying…`.
- Prompts show in the status line in yellow: `allow <operation>? (y/n, esc to stop)`.
- Shift+Tab cycles `none → read-only → read-write`, saved to `[repl] permission`.

## Removed

`src/repl_driver.py`, the classifier (request, verdict notes, `[classifier] prompt`), and approval modes. The synthetic-call and separate `reasoning()` approval exemptions remain.
