from pathlib import Path as _SearchPath
import re as _search_re


# search's public 'glob' parameter shadows the function; retain a private callable.
_glob_paths = glob


# Use the same traversal as glob so exclusions and symlink handling stay consistent.
def search(
    pattern: str,
    path: str | None = None,
    glob: str | None = None,
    exclude: tuple[str, ...] = (".git", "target", "node_modules"),
) -> list[dict[str, str | int]]:
    """Find regex matches in UTF-8 files beneath path (default '.').

    Return one {'path', 'line', 'text'} record per matching line, ordered by path
    then 1-based line number; text excludes the line ending. Paths preserve the
    supplied relative root. glob filters paths relative to that root (default
    '**/*'). Absolute paths and '..' components in path or glob are rejected.
    Excluded entry names and directory symlinks are skipped. Regex, filesystem,
    permission, and text-decoding errors propagate rather than hiding omissions.
    """
    paths = _glob_paths("**/*" if glob is None else glob, path, exclude)
    expression = _search_re.compile(pattern)
    results: list[dict[str, str | int]] = []
    for name in paths:
        entry = _SearchPath(name)
        if entry.is_file():
            for number, text in enumerate(entry.read_text().splitlines(), 1):
                if expression.search(text) is not None:
                    results.append({"path": name, "line": number, "text": text})
    return results
