"""REPL driver, embedded in the harness binary and run as `python3 -u -c <this>`.

Protocol: JSON lines over the process's original stdin/stdout.
  host -> driver: {"code": "...", "prompt_tokens": n | null}
  driver -> host: {"output": "..."}   output as it is written
                  {"done": true, "value": "..." | null, "error": "..." | null}

The code's stdin is /dev/null, and fds 1 and 2 (inherited by any subprocess the
code starts) go to a pipe this driver reads, so program output can never corrupt
the protocol.
"""

import os

# A new session with no controlling terminal: programs that open /dev/tty (sudo,
# ssh, credential prompts) fail immediately instead of drawing over the TUI and
# hanging. This also makes the driver a process-group leader, so the host can kill
# it and everything it starts as one group.
try:
    os.setsid()
except OSError:
    pass

import ast
import builtins
import codecs
import inspect
import json
import queue
import sys
import textwrap
import threading
import time
import traceback
import types

OUTPUT_LIMIT = int(os.environ.get("HARNESS_OUTPUT_LIMIT", "100000"))
# Absolute path of `replib/` in the harness directory, not the working directory.
LIBRARY_DIR = os.environ["HARNESS_LIBRARY_DIR"]

proto_in = os.fdopen(os.dup(0), "r", encoding="utf-8")
proto_out = os.fdopen(os.dup(1), "w", encoding="utf-8")
send_lock = threading.Lock()


def send(message):
    with send_lock:
        proto_out.write(json.dumps(message) + "\n")
        proto_out.flush()


os.dup2(os.open(os.devnull, os.O_RDONLY), 0)
capture_r, capture_w = os.pipe()
os.dup2(capture_w, 1)
os.dup2(capture_w, 2)
# Private write end for the sync marker, so code that redirects or closes fd 1
# can't stop the marker from arriving.
sync_fd = capture_w

# Written to the capture pipe after each call. Pipe writes are ordered, so once the
# reader sees it, all output written before it has been forwarded.
SYNC = b"\x00harness-sync-" + os.urandom(8).hex().encode() + b"\x00"
synced = threading.Event()


def read_output():
    decoder = codecs.getincrementaldecoder("utf-8")("replace")
    buf = b""

    def forward(data):
        text = decoder.decode(data)
        if text:
            send({"output": text})

    while True:
        data = os.read(capture_r, 65536)
        if not data:
            return
        buf += data
        while (i := buf.find(SYNC)) >= 0:
            forward(buf[:i])
            buf = buf[i + len(SYNC):]
            synced.set()
        # Hold back only a tail that could be the start of the sync marker.
        hold = next(
            (k for k in range(min(len(SYNC) - 1, len(buf)), 0, -1) if buf.endswith(SYNC[:k])),
            0,
        )
        forward(buf[: len(buf) - hold])
        buf = buf[len(buf) - hold:]


threading.Thread(target=read_output, daemon=True).start()

registry = {}


def register(fn):
    """Decorator that adds a library function to the REPL and to help()."""
    registry[fn.__name__] = fn
    return fn


_missing = object()


def help(obj=_missing):
    if obj is not _missing:
        return builtins.help(obj)
    print(
        f"help()\n"
        f"Output limit: the output of each call returned to you is truncated to "
        f"{OUTPUT_LIMIT:,} bytes. Keep large data in variables or files rather than "
        f"printing it."
    )
    if not registry:
        print("No registered functions.")
        return
    print("Registered functions:")
    for name, fn in registry.items():
        print(f"\n{name}{inspect.signature(fn)}")
        doc = inspect.getdoc(fn)
        if doc:
            print(textwrap.indent(doc, "    "))


namespace = {"__name__": "__main__", "__builtins__": builtins, "help": help}
# FYI() is the only implementation of the situational-awareness snapshot: the app's
# synthetic call at the start of each turn runs it here too. Deliberately not
# registered, so help() doesn't list it.
MODEL = os.environ.get("HARNESS_MODEL", "unknown")
# Updated from each request, so FYI() reports the count current at call time.
prompt_tokens = None


def _fyi():
    now = time.localtime()
    # `%:z` needs Python 3.12+; insert the colon into `%z` by hand instead.
    offset = time.strftime("%z", now)
    offset = f"{offset[:3]}:{offset[3:]}" if len(offset) == 5 else offset
    date = time.strftime("%a %b %d %H:%M:%S ", now) + offset + time.strftime(" %Y", now)
    lines = ["FYI()", f"Date: {date}"]
    # No count until a response has reported usage; omit the line rather than print a placeholder.
    if prompt_tokens is not None:
        lines.append(f"Prompt tokens: {prompt_tokens}")
    lines.append(f"Model: {MODEL}")
    print("\n".join(lines))


namespace["FYI"] = _fyi


def load_library():
    """Run every `replib/*.py` with `register` available; return load errors."""
    errors = []
    if os.path.isdir(LIBRARY_DIR):
        for name in sorted(os.listdir(LIBRARY_DIR)):
            if not name.endswith(".py"):
                continue
            path = os.path.join(LIBRARY_DIR, name)
            module = types.ModuleType(f"replib.{name[:-3]}")
            module.__file__ = path
            module.register = register
            try:
                with open(path, encoding="utf-8") as f:
                    exec(compile(f.read(), path, "exec"), module.__dict__)
            except BaseException:
                errors.append(f"{path} failed to load:\n{traceback.format_exc()}")
    namespace.update(registry)
    return errors


def run(code):
    """Execute like a REPL: the value of a trailing expression is returned."""
    try:
        tree = ast.parse(code, "<repl>", "exec")
        last = None
        if tree.body and isinstance(tree.body[-1], ast.Expr):
            last = ast.Expression(tree.body.pop().value)
        exec(compile(tree, "<repl>", "exec"), namespace)
        if last is not None:
            value = eval(compile(last, "<repl>", "eval"), namespace)
            if value is not None:
                namespace["_"] = value
                return repr(value), None
        return None, None
    except BaseException as e:  # Includes SystemExit: exit() must not end the REPL.
        # Drop this function's frame so the traceback starts at the model's code.
        tb = e.__traceback__.tb_next if e.__traceback__ else None
        return None, "".join(traceback.format_exception(type(e), e, tb))


# Read requests on a thread from the start, so the host's writes never wait on
# library loading or on running code.
requests = queue.Queue()


def read_requests():
    for line in proto_in:
        requests.put(line)
    requests.put(None)


threading.Thread(target=read_requests, daemon=True).start()

load_errors = load_library()

while (line := requests.get()) is not None:
    request = json.loads(line)
    prompt_tokens = request.get("prompt_tokens")
    if load_errors:
        os.write(sync_fd, ("\n".join(load_errors) + "\n").encode())
        load_errors = []
    value, error = run(request["code"])
    # The code may have closed or replaced sys.stdout/sys.stderr; flush the
    # originals and ignore failures so the driver survives.
    for stream in (sys.stdout, sys.stderr, sys.__stdout__, sys.__stderr__):
        try:
            stream.flush()
        except Exception:
            pass
    os.write(sync_fd, SYNC)
    synced.wait()
    synced.clear()
    send({"done": True, "value": value, "error": error})
