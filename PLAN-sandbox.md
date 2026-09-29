# Sandbox implementation plan

Spec: `SPEC-sandbox.md`. Monty reference source: `monty/` (v1.0.0, reference only, not built).

Implementation is complete within the reduced acceptance scope recorded in Phase 4.

## 1. Config and dependencies

Implemented. Formatting and TOML parsing checks passed; the integrated build passed in Phase 3. Validation results are recorded in Phase 4.

Deferred-feature cleanup complete: removed `lib_auto_allow` and `save_lib_auto_allow` from `src/config.rs` and the setting from `config.toml.example`. Formatting and example TOML parsing checks passed.

- `Cargo.toml`: `monty-pool = "=1.0.0"`; `monty-proto = { version = "=1.0.0", features = ["worker"] }`; `monty-alloc = { version = "=1.0.0", features = ["exit-code"] }`; `monty-types = "=1.0.0"`; `ruff_python_parser` and `ruff_python_ast` `=0.0.14` (Monty's versions); `libc`.
- `Cargo.lock`: retain `salsa`, `salsa-macros`, and `salsa-macro-rules` at `0.28.2` for compatibility with Ruff `0.0.14`.
- `src/config.rs`: remove `Classifier` and `ApprovalMode`. Add `Permission { None, ReadOnly, ReadWrite }` (`next()`: none → read-only → read-write → none), `max_memory_mb`, `Commands { deny, env_filter, allow: BTreeMap<String, PathBuf> }` with non-absolute `allow` paths rejected. Require explicit `tool_reasoning`, `tool_description`, `output_limit`, `permission`, and `max_memory_mb`; missing settings fail startup, with no runtime defaults. `save_permission` via `save_setting`.
- `config.toml.example`: matching changes.

## 2. Sandbox runtime

Implemented. Rust formatting checks, starter-library Python syntax checks, source review, and the integrated build passed. Validation results are recorded in Phase 4.

### Worker mode

- The pool spawns `<exe> subprocess` with the environment cleared, stdin/stdout piped, stderr inherited (`monty/crates/monty-pool/src/worker.rs:184`).
- `src/main.rs`: `#[global_allocator] static ALLOC: monty_alloc::LimitedAllocator = monty_alloc::LimitedAllocator;` (no limit until the worker sets one) and a hidden `subprocess` subcommand.
- `src/worker.rs`: first redirect fd 2 to `/dev/null` (`libc::dup2`), then port `monty/crates/monty-runtime/src/subprocess.rs`: 16 MiB-stack thread, panic hook writing `fatal_error_event`, `FrameReader` loop over `Child::handle`, `apply_memory_limit` (`memory_limit_with_headroom` + `monty_alloc::set_hard_limit`), same exit codes.

### REPL core (`src/repl.rs` rewritten)

- At startup: `Pool::new(PoolConfig::subprocess(current_exe()))`, `min_processes = 1`, `max_processes = 1`.
- Per turn: `pool.checkout(&ReplConfig { limits: Some(ResourceLimits::default().max_memory(bytes).max_suspensions(usize::MAX)), type_check: false, type_check_stubs: None, .. })`; `bytes` is checked MiB-to-byte conversion. Construct `MountSpec::new(repo, repo, MountSpecMode::ReadWrite)` once at startup and clone it for every feed, preserving the opened directory handle.
- The interpreter's surviving state is authoritative after ordinary exceptions; completed effects remain. Static annotations are not enforced. Host argument validation and per-operation permissions remain mandatory.
- Library load: feed each `replib/*.py` in name order; failures are prefixed to the first call's output; failed files are remembered for `help()`.
- A driver task owns the `Checkout`. App → driver: call `{code, prompt_tokens}`, prompt answers. Driver → app: output text, prompt request, failure record, call result. Esc/exit aborts the task (dropping the checkout kills the worker) and kills any `run()` process group.
- Turn events: `Complete(v)` → `v.py_repr()` unless `None`; `Err(PoolError::Runtime(exc))` → `exc` Display (CPython traceback), except the memory-limit `MemoryError` → memory-limit text and a new session; `Err(PoolError::Typing(d))` → `d` and a `type check` failure; other `PoolError` → crash text, new checkout and library for the next call; `NameLookup` for a host function name → `MontyObject::function(name, None)`, else `NameLookupResult::Undefined`; `FunctionCall` → host functions; `OsCall` → permissions.
- Host functions' printed text is inserted into the call's output at the call (Monty flushes `print` before every host call).
- `checked_reply` checks direct host replies with Monty's protocol frame-size API before resuming. Oversized replies raise a catchable error without retrying host effects or stranding the suspension.

### Permissions (`src/hostfs.rs`)

- Path arguments are `MontyNode::Path` via `monty_types::unstable::node` (no stability guarantee). Writes: `Path.write_*`, `Path.append_*`, `Path.mkdir`, `Path.unlink`, `Path.rmdir`, `Path.rename`, `open` with a creating mode (`FileMode` via `FromStr`, `.create()`).
- Stateless `hostfs::handle` checks normalized mount coverage and the current permission; rename checks both endpoints. Outside paths are denied without prompting.
- All permitted filesystem calls use `checkout.resume_from_mounts`. Unmodified Monty is authoritative for traversal and symlink containment; there are no outside host handlers or approval caches. Uncovered calls receive `ResumeValue::NotHandled`.
- `Getenv` → the default argument; `GetEnviron` → `{}`; any other OS call → `ResumeValue::NotHandled`.
- Denial: `ResumeValue::Error(MontyException::new(ExcType::PermissionError, Some(msg)))` plus a `denied` failure record.

### Host functions (`src/host.rs`)

- `FYI()`: current output. `help()`: output limit, `repl-notes.md`, host functions, library listing (`ruff_python_parser` module parse: top-level functions not starting with `_`, parameter source text, docstring), `allow` names.
- `run(argv, cwd=None)`: arguments via `CallArgs::arg`/`kwarg`. Resolution per spec. `tokio::process::Command`: `pre_exec` `libc::setsid()`, `env_clear()` plus the app's environment minus `env_filter`, stdin null, stdout/stderr piped. Signal exit → negative signal number. Abort → `libc::killpg`.
- `lib_source(name)`: return the contents of `replib/<name>.py`.
- Deferred: `lib_define`, `lib_remove`, scratch validation, `lib_auto_allow`, and `/lib-auto`.
- `STUBS` supplies signatures for `help()`: `FYI() -> None`, `help() -> None`, `run(argv: list[str], cwd: str | None = None) -> dict[str, Any]`, `lib_source(name: str) -> str`. It is not passed to a type checker.
- Error and denial messages as written in the spec.

### Starter library

- `replib/glob.py`: `glob(pattern, path=None, exclude=(".git", "target", "node_modules"))` → sorted relative string paths preserving the supplied root.
- `replib/search.py`: `search(pattern, path=None, glob=None, exclude=(".git", "target", "node_modules"))` → list of `{"path", "line", "text"}`.
- `search.py` defines a private alias for public `glob` to avoid its parameter shadowing the function.
- Both default to `.` and skip directory symlinks; path inputs reject absolute paths and `..` components. `reasoning.py` is a typed no-op without `@register`.

## 3. Application integration

Implemented. Live configuration migration is complete. `cargo build --locked --bin harness` and formatting checks passed after disabling static checking. Validation results are recorded in Phase 4.

- Create `repl::Runtime::new(&settings)` before the TUI. Use `Runtime::start` per turn, `Repl::finish` for normal completion, `kill`/drop for cancellation, and `Runtime::close` on shutdown. Permission changes call `Runtime::set_permission`.
- `src/app.rs`: calls run without an approval phase (the `reasoning()` shortcut stays); `Phase::Running` holds a pending prompt; `y`/`n`/Esc answer it.
- Removed `client::classify`, `Verdict`, verdict notes, `ApprovalMode` handling, and `src/repl_driver.py`. Required `[repl] tool_description` flows through `Settings.repl_tool_description` to every REPL tool definition verbatim.
- Preserve the approval exemption contract in `SPEC.md`: all present and future synthetic tool calls, including `FYI()` and `help()`, never require approval, whether app-generated or explicitly called by the model, at every permission level. Preserve the separate exemption for non-synthetic `reasoning()`. Unrelated code bundled with either, including code evaluated in arguments, follows normal permission and approval rules.
- Status line and prompt text per spec. Shift+Tab → `Permission::next` + `save_permission`.
- `src/failures.rs`: append `{"time", "session", "kind", "message", "code"}` to `~/.harness/failures.jsonl`; counts per `(kind, first line of message)` loaded at startup, incremented in memory; yellow note `<kind>: <first line> (seen N times)` under the call.

## 4. Completion and validation

Complete within the approved reduced scope. Essential automated checks, the user-reported terminal smoke test, README completion, and status updates are complete. Do not add further automated validation without approval.

- `README.md` updated for Monty, configuration, permissions, commands, libraries, recovery, and failure notes; obsolete CPython/classifier instructions removed.
- `PLAN.md` records Sandbox as Done, with library-authoring features still deferred and the approval exemption contract retained.
- Completed with approval: migrated `~/.harness/config.toml` (resolves to `/Users/goon/project/harness/config.toml`), removing `[classifier]` and `[repl] approval_mode` and adding the required sandbox settings.
- Passed through the application runtime: library loading, `FYI()`, `help()`, `lib_source`, basic `glob`/`search` results, reassignment, private names and imports across calls, surviving state/output after exceptions, and fresh-turn reset. Static annotations are not enforced.
- Passed: invalid host arguments, command deny-list enforcement and denial events, repository reads denied at `none`, reads allowed and writes denied at `read-only`, writes allowed at `read-write`, and direct outside-path denial. Temporary validator source and integration fixtures were removed.
- `cargo build --locked --bin harness` and `rustfmt --edition 2024 --check src/repl.rs src/host.rs` passed from `/Users/goon/project/harness`.
- Passed configuration checks: required settings, invalid and unknown values, removed-key rejection, permission and tool-reasoning persistence in normal and inline tables with unrelated text preserved, credential resolution, shell precedence, and no `.env` export. Temporary validator source and scratch fixtures were removed.
- Passed output/result checks through application handlers with injected events: byte limits and Unicode boundaries, empty output, output/value/error ordering, one saved result per completed or failed call, and interruption results for active and queued calls. Saved JSON matched memory. Temporary source and fixtures were removed.
- Passed containment checks at all permission levels: in-repository absolute and parent paths, escaping reads/writes/open/rename, relative and absolute symlink behavior, and lexical resolution. Outside fixtures stayed unchanged. The retained mount survived root rename/symlink replacement across calls and turns. Temporary source and fixtures were removed.
- Passed all six directed permission transitions within one REPL call: the runtime level changed at a suspended command prompt, the command was denied, prior effects remained, and subsequent reads/writes used the new level. Temporary source and fixtures were removed; Shift+Tab UI behavior is unverified.
- Passed command checks: allow/deny precedence, PATH resolution, approval/rejection at all permission levels, literal arguments, cwd rules and outside-directory approvals, exact environment filtering, null stdin, process-session isolation, buffered stdout/stderr, exit/signal codes, and UTF-8 replacement. Rejected helpers did not execute. Temporary source and fixtures were removed.
- Passed library error checks: absent/invalid directories, unreadable and invalid-UTF-8 entries, syntax/runtime/unsupported/permission failures, first-call diagnostics, continued loading, failed/private function omission from help, source retrieval errors, notes read errors/recovery, and new-turn reload after repair. Temporary source and fixtures were removed.
- Passed recovery checks: killed worker and 64 MiB allocation against a 32 MiB budget report state loss, preserve partial output/completed effects, replace the worker, and reload libraries without replay. Python cannot catch the resource failure; ordinary exceptions retain state. Later calls and turns succeed. Temporary source and fixtures were removed.
- Passed host-reply checks around the 256 MiB protocol limit: a below-limit reply arrived intact; an oversized reply raised a catchable error, preserved REPL state and completed effects, and did not retry the command. Later host calls succeeded. Temporary source and fixtures were removed.
- Passed runtime cancellation checks for explicit kill and handle drop: busy computation and approval suspensions terminate without continuation; host-command leaders and two descendant generations disappear; new turns remain usable; closing the runtime leaves no test workers. Temporary source and fixtures were removed.
- Terminal smoke test completed by user report. Shared transcripts confirm conversation startup, synthetic calls, library listing, working-directory lookup, successful command execution, and user denial; the user reported that stopping the deliberately long-running call and exiting worked.
- Skipped with approval and unverified: exhaustive application keyboard/signal/UI checks, failure-log persistence/counts, and the complete approval-exemption matrix. The PTY attempt was inconclusive; its artifacts were removed. The specification's approval-exemption contract remains unchanged.
