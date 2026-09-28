# Sandbox implementation plan

Spec: `SPEC-sandbox.md`. Monty reference source: `monty/` (v1.0.0, reference only, not built). Each phase ends with `cargo build` and a manual check.

Needs user approval when reached:
- Phase 1: the `Cargo.toml` dependency additions and the `config.toml.example` edit.
- Phase 1: migration of the live `~/.harness/config.toml` (remove `[classifier]` and `[repl] approval_mode`; add the new keys). The old file fails to load after phase 1 (unknown keys are rejected).
- Phase 6: the new REPL tool description text in `src/client.rs`.

## 1. Config and dependencies

- `Cargo.toml`: `monty-pool = "=1.0.0"`; `monty-proto = { version = "=1.0.0", features = ["worker"] }`; `monty-alloc = { version = "=1.0.0", features = ["exit-code"] }`; `monty-types = "=1.0.0"`; `ruff_python_parser` and `ruff_python_ast` `=0.0.14` (Monty's versions); `libc`.
- `src/config.rs`: remove `Classifier` and `ApprovalMode`. Add `Permission { None, ReadOnly, ReadWrite }` (`next()`: none → read-only → read-write → none), `lib_auto_allow`, `max_memory_mb` (default 2048), `Commands { deny, env_filter, allow: BTreeMap<String, PathBuf> }` with non-absolute `allow` paths rejected. `save_permission` and `save_lib_auto_allow` via `save_setting`.
- `config.toml.example`: matching changes.

## 2. Worker mode

- The pool spawns `<exe> subprocess` with the environment cleared, stdin/stdout piped, stderr inherited (`monty/crates/monty-pool/src/worker.rs:184`).
- `src/main.rs`: `#[global_allocator] static ALLOC: monty_alloc::LimitedAllocator = monty_alloc::LimitedAllocator;` (no limit until the worker sets one) and a hidden `subprocess` subcommand.
- `src/worker.rs`: first redirect fd 2 to `/dev/null` (`libc::dup2`), then port `monty/crates/monty-runtime/src/subprocess.rs`: 16 MiB-stack thread, panic hook writing `fatal_error_event`, `FrameReader` loop over `Child::handle`, `apply_memory_limit` (`memory_limit_with_headroom` + `monty_alloc::set_hard_limit`), same exit codes.

## 3. REPL core (`src/repl.rs` rewritten)

- At startup: `Pool::new(PoolConfig::subprocess(current_exe()))`, `min_processes = 1`, `max_processes = 2` (the second is `lib_define`'s scratch session).
- Per turn: `pool.checkout(&ReplConfig { limits: Some(ResourceLimits::default().max_memory(mb << 20).max_suspensions(usize::MAX)), type_check: true, type_check_stubs: Some(STUBS), .. })`. Every feed passes `MountSpec::new(repo, repo, MountSpecMode::ReadWrite)` (repo = real absolute working directory, built once).
- Library load: feed each `replib/*.py` in name order; failures are prefixed to the first call's output; failed files are remembered for `help()`.
- A driver task owns the `Checkout`. App → driver: call `{code, prompt_tokens}`, prompt answers. Driver → app: output text, prompt request, failure record, call result. Esc/exit aborts the task (dropping the checkout kills the worker) and kills any `run()` process group.
- Turn events: `Complete(v)` → `v.py_repr()` unless `None`; `Err(PoolError::Runtime(exc))` → `exc` Display (CPython traceback), except the memory-limit `MemoryError` → memory-limit text and a new session; `Err(PoolError::Typing(d))` → `d` and a `type check` failure; other `PoolError` → crash text, new checkout and library for the next call; `NameLookup` for a host function name → `MontyObject::function(name, None)`, else `NameLookupResult::Undefined`; `FunctionCall` → phase 5; `OsCall` → phase 4.
- Host functions' printed text is inserted into the call's output at the call (Monty flushes `print` before every host call).
- `src/app.rs`: calls run without an approval phase (the `reasoning()` shortcut stays); `Phase::Running` holds a pending prompt; `y`/`n`/Esc answer it.

## 4. Permissions (`src/hostfs.rs`)

- Path arguments are `MontyNode::Path` via `monty_types::unstable::node` (no stability guarantee). Writes: `Path.write_*`, `Path.append_*`, `Path.mkdir`, `Path.unlink`, `Path.rmdir`, `Path.rename`, `open` with a creating mode (`FileMode` via `FromStr`, `.create()`).
- Location is decided on the resolved path (nearest existing ancestor for new paths).
- Inside the repo: level check, then `checkout.resume_from_mounts`.
- Outside: prompt, cached per `(path, read|write)` for the current call; approved operations run with `std::fs`, returning `MontyObject::{string, bytes, bool, none, path, list, file_handle}` and `monty_types::os::{file_stat, dir_stat, stat_result}`.
- `Getenv` → the default argument; `GetEnviron` → `{}`; any other OS call → `ResumeValue::NotHandled`.
- Denial: `ResumeValue::Error(MontyException::new(ExcType::PermissionError, Some(msg)))` plus a `denied` failure record.

## 5. Host functions (`src/host.rs`)

- `FYI()`: current output. `help()`: output limit, `repl-notes.md`, host functions, library listing (`ruff_python_parser` module parse: top-level functions not starting with `_`, parameter source text, docstring), `allow` names.
- `run(argv, cwd=None)`: arguments via `CallArgs::arg`/`kwarg`. Resolution per spec. `tokio::process::Command`: `pre_exec` `libc::setsid()`, `env_clear()` plus the app's environment minus `env_filter`, stdin null, stdout/stderr piped. Signal exit → negative signal number. Abort → `libc::killpg`.
- `lib_define`: parse check; scratch checkout with the same `ReplConfig`, library fed, then `source`; `finish()`; prompt (showing the source as a note under the call) unless `lib_auto_allow`; write. `lib_remove`, `lib_source`.
- `STUBS` (`.pyi` text): `FYI() -> None`, `help() -> None`, `run(argv: list[str], cwd: str | None = None) -> dict[str, Any]`, `lib_define(source: str) -> None`, `lib_remove(name: str) -> None`, `lib_source(name: str) -> str`.
- Error and denial messages as written in the spec.

## 6. App and UI

- Remove `client::classify`, `Verdict`, verdict notes, and `ApprovalMode` handling. Update the REPL tool description.
- Status line and prompt text per spec. Shift+Tab → `Permission::next` + `save_permission`. `/lib-auto`: `Action::ToggleLibAuto` in `src/commands.rs`.
- `src/failures.rs`: append `{"time", "session", "kind", "message", "code"}` to `~/.harness/failures.jsonl`; counts per `(kind, first line of message)` loaded at startup, incremented in memory; yellow note `<kind>: <first line> (seen N times)` under the call.

## 7. Starter library

- `replib/glob.py`: `glob(pattern, path=None, exclude=(".git", "target", "node_modules"))`.
- `replib/search.py`: `search(pattern, path=None, glob=None, exclude=(".git", "target", "node_modules"))` → list of `{"path", "line", "text"}`.
- Both in Monty's subset (`Path.iterdir`, `re`, no generators). Confirm `replib/reasoning.py` loads.

## 8. Docs

- `README.md`: config, permission levels, commands, library functions, failure notes; remove the classifier and approval modes.
- `PLAN.md`: Done entry; remove Next #2 and the Open item on the `FYI()`/`help()` approval exemption.
