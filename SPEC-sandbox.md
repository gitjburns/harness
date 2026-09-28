# Sandboxed REPL

Designed, not implemented. Replaces the CPython REPL with Monty (`monty-pool` 1.0.0), a Python-subset interpreter with no filesystem, environment, network, or process access of its own. Everything outside the sandbox goes through the app. Merged into `SPEC.md` when implemented; sections below replace the matching parts of `SPEC.md`.

Every error, denial, and failure the app reports to the model states what happened, why, and any consequence for the session (such as lost state). Messages below are the standard.

## Config

Removed: `[classifier]`, `[repl] approval_mode`. Added:

```toml
[repl]
permission = "none"          # "none" | "read-only" | "read-write"; default "none"; rewritten by Shift+Tab
lib_auto_allow = false       # default false; rewritten by /lib-auto
max_memory_mb = 2048         # REPL worker memory limit in MiB; default 2048

[commands]
deny = ["git", "rm"]         # command names; always denied
env_filter = ["GITHUB_TOKEN"] # exact variable names removed from commands' environment

[commands.allow]             # name the model calls = absolute path that runs
cargo = "/Users/me/.cargo/bin/cargo"
```

An `allow` value that isn't an absolute path is a config error. A name in both `deny` and `allow` is denied.

## REPL

- Monty worker: the harness binary run as a hidden worker subcommand (`monty_proto::worker::Child` over stdio, `monty_alloc::LimitedAllocator` as global allocator), launched via `current_exe()` by a `monty_pool::Pool` with one idle worker.
- A turn checks out a worker (fresh session), feeds `replib/*.py`, then runs the turn's calls; state persists across the turn's calls. The turn's end finishes the session and returns the worker. Esc or exit drops the checkout, discarding the worker. A crashed worker ends the call with `[REPL worker crashed: <reason>. The REPL was restarted: every variable, import, and function defined earlier in this turn is gone; library functions are reloaded.]`; the next call gets that new session.
- Type checking is on for every fed snippet, with stubs for the host functions. A rejected snippet does not run; its result is the diagnostics.
- Limits: `max_suspensions = usize::MAX`; `max_memory` from `[repl] max_memory_mb`; no time limits (Esc stops). Hitting the memory limit discards the worker and ends the call with `[REPL memory limit of <N> MiB exceeded. The REPL was restarted: every variable, import, and function defined earlier in this turn is gone; library functions are reloaded.]`.
- The repo (working directory) is mounted read-write at its real absolute path, which is also the sandbox working directory. `os.getenv`/`os.environ` see an empty environment.
- Result: `print` output in order, then the repr of a trailing expression if not `None`, then a traceback if the code raised; `(no output)` if empty; truncated at `output_limit` as before.
- Exact `reasoning(<string literal>)` calls are still answered `(no output)` without running.

## Permissions

Every file operation reaches the app as an OS call and is checked before it is serviced:

- Inside the repo: `none` denies all; `read-only` denies writes; `read-write` allows all.
- Outside the repo: asks, in every level. Approved operations are performed by the app on the host.
- A change of level applies from the next operation, including mid-call.
- Every denial raises `PermissionError` at the call, e.g. `write /repo/x: denied: the permission level is read-only`, `read /etc/hosts: denied by the user`, `run 'git': denied: 'git' is on the command deny list`.

## Commands

`run(argv, cwd=None)` → `{"exit_code": int, "stdout": str, "stderr": str}`; prints nothing.

- `argv[0]` is a bare command name; one containing `/` raises `ValueError: run takes a command name, not a path: './build.sh'`.
- In `deny`: denied. In `allow`: runs the configured path without asking. Otherwise the app resolves the name through its own `PATH` and asks `allow <path> <args>?`; not found raises `FileNotFoundError: run: no command named 'npm' is configured or found on the host`. Independent of the permission level.
- `cwd` defaults to the repo root; a directory outside the repo asks.
- The program runs directly (no shell), stdin `/dev/null`, in its own session with no controlling terminal, with the app's environment minus `env_filter`. Esc kills its process group. No timeout. Output is decoded as UTF-8 with replacement and never shown live.

## Library

- `~/.harness/replib/*.py` is Monty code, fed in name order at session start with `register` in scope; `@register` adds a function to `help()`. Load errors are printed in the first call's output. The repo ships `reasoning.py`, `glob.py`, and `search.py`.
- `lib_define(source)`: `source` defines one function. It runs in the current session; if it raises or doesn't define exactly one function, nothing is written. Otherwise the app writes `replib/<name>.py` as `@register` followed by `source`, replacing any existing file.
- `lib_remove(name)`: deletes `replib/<name>.py`; the function stays defined until the turn ends.
- `lib_source(name)`: returns the file's contents.
- `lib_define` and `lib_remove` ask unless `lib_auto_allow` is on.

## Host functions

- `FYI()`: unchanged output.
- `help()`: the output limit, then `~/.harness/repl-notes.md` if present, then host functions, registered library functions (signature and docstring), and the `allow` command names.
- `run`, `lib_define`, `lib_remove`, `lib_source`: above.

## Failures

- Type-check rejections, app denials, and `NotImplementedError` from Monty are appended to `~/.harness/failures.jsonl`: `{"time", "session", "kind", "message", "code"}`, `kind` one of `type check`, `denied`, `unsupported`.
- Counts per `(kind, first line of message)` are loaded at startup and kept in memory.
- Each failure shows a yellow note under its call: `<kind>: <first line> (seen N times)`, N including this one. Notes are never saved or sent.

## Terminal UI

- Status line: `<model> | N tokens | <permission>`, ` | lib auto` while on, ` | tool reasoning` while on; activity as before without ` · classifying…`.
- Prompts show in the status line in yellow: `allow <operation>? (y/n, esc to stop)`.
- Shift+Tab cycles `none → read-only → read-write`, saved to `[repl] permission`.
- `/lib-auto`: switch `lib_auto_allow`, saved to `config.toml`.

## Removed

`src/repl_driver.py`, the classifier (request, verdict notes, `[classifier] prompt`), approval modes, and the `FYI()`/`help()` approval exemption.
