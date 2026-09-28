# Principles

## Core Principles

- **Simplicity**: Proven patterns over cleverness. Simplicity means
  straightforward implementation, not reduced capability.
- **Transparency**: Do not hide operationally relevant data. Summaries may
  supplement raw data, but must not replace or obscure it. Secrets are the
  exception.
- **Accuracy**: Do not present guesses as facts. Use authoritative data. If
  only an estimate is possible, label it clearly and never use it as a
  correctness source of truth.
- **Quality**: Do not patch symptoms when the source of truth is wrong. Fix the
  authoritative behavior. If a narrow fix is unavoidable, state why and do not
  disguise it as a structural solution.
- **Code quality over tool satisfaction**: Fix errors to improve the code, not
  merely to silence the compiler, linter, or type checker.
- **No hidden fallback logic**: Do not add silent fallback paths that mask broken
  primary behavior. Fail clearly or surface degraded behavior explicitly.

## Source Of Truth

- Each kind of data has one authoritative source of truth. Derived, cached, or
  generated representations are produced from it, never hand-maintained as a
  stale parallel copy.
- Every stated fact must trace to the source of truth. Derived estimates,
  metadata, or counts are never presented as authoritative answers.

## Code Comments

- Every function must have a useful comment immediately before it explaining
  its purpose or key invariant.
- Comments are not limited to functions. Include them anywhere the code is not
  completely intuitive or clear to someone unfamiliar with the codebase.
- Comments must explain intent, invariants, side effects, ownership boundaries,
  failure modes, or non-obvious design constraints. Do not write comments that
  merely restate names, types, parameters, or syntax.
- Add brief comments at semantic boundaries inside functions when behavior is
  not obvious from local code alone: validation, normalization, persistence,
  external calls, and intentional error behavior.
- When data changes meaning across a pipeline, comment the boundary where the
  meaning changes.
- Update nearby comments when changing code. Stale comments are bugs.

## Configuration

- Operational configuration lives in a single configuration file.
- Secrets live outside version control (environment, secrets file, or similar)
  and must never be committed.
- Missing config files, missing required keys, unknown keys, and missing required
  secrets are fatal startup errors.
- Never add runtime defaults for operational settings. Defaults belong in an
  example configuration file, where the operator can see and edit them.
- Environment overrides must be explicit and documented. They must not silently
  change correctness, data sources, or safety limits.

## Structural Fixes

- Fix behavior at the authoritative source. Do not patch symptoms locally when
  the source of truth is wrong.
- When logic is spread across multiple places, consolidate toward one source of
  truth instead of adding another branch or copy.
- If a second issue appears in the same subsystem, stop and reassess the
  structure before continuing.
- If a narrower fix is chosen instead of the structural fix, explicitly explain
  why.
- Do not add branching, fallback paths, duplicated logic, or local special cases
  when the correct fix is to clarify ownership.

## Error Messages

- Every error condition must produce a human-readable message stating what
  failed and, where known, the input, row, or key involved.
- Python tracebacks are avoided whenever possible. Catch expected failures at
  the boundary where the message can be made specific, and exit nonzero.
- A traceback is acceptable only for a genuine bug, never for bad input,
  missing files, or unreachable services.
