#!/usr/bin/env python3
"""Install in a trusted Linux workspace image; communicate over virtio serial only."""
import base64
import json
import os
from pathlib import Path
import selectors
import signal
import subprocess
import stat
import time

MAX_OUTPUT = 256 * 1024


def export_artifacts(root):
    exports = root / "exports"
    if not exports.exists():
        return []
    if exports.is_symlink() or not exports.is_dir():
        raise ValueError("Artifact exports must be a real directory")
    artifacts, total, visited = [], 0, 0
    for directory, folders, files in os.walk(exports, followlinks=False):
        visited += len(folders) + len(files)
        if visited > 1000:
            raise ValueError("Too many artifact entries")
        for name in folders:
            if (Path(directory) / name).is_symlink():
                raise ValueError("Artifact symlinks are forbidden")
        for name in sorted(files):
            path = Path(directory) / name
            relative = path.relative_to(exports)
            if any(part.startswith(".") for part in relative.parts) or len(artifacts) >= 32:
                raise ValueError("Invalid artifact name or too many artifacts")
            descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
            with os.fdopen(descriptor, "rb") as source:
                metadata = os.fstat(source.fileno())
                if not stat.S_ISREG(metadata.st_mode) or metadata.st_size > 1024**2:
                    raise ValueError("Artifact is not a regular file under 1 MiB")
                data = source.read(1024**2 + 1)
            total += len(data)
            if len(data) > 1024**2 or total > 4 * 1024**2:
                raise ValueError("Artifacts exceed export limits")
            artifacts.append({"path": str(relative), "data": base64.b64encode(data).decode()})
    return artifacts


def run(request):
    if request.get("version") != 1:
        raise ValueError("Unsupported protocol")
    root = Path("/workspace")
    root.mkdir(mode=0o700, parents=True, exist_ok=True)
    files = request.get("files", [])
    if len(files) > 1000:
        raise ValueError("Too many files")
    total = 0
    for entry in files:
        relative = Path(entry["path"])
        if relative.is_absolute() or ".." in relative.parts:
            raise ValueError("Invalid workspace path")
        target = root / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        data = base64.b64decode(entry["data"], validate=True)
        total += len(data)
        if total > 8 * 1024 * 1024:
            raise ValueError("Workspace too large")
        target.write_bytes(data)
    process = subprocess.Popen(
        ["/bin/sh", "-c", request["command"]], cwd=root,
        env={"PATH": "/usr/local/bin:/usr/bin:/bin", "HOME": "/home/muse", "LANG": "C.UTF-8"},
        stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        start_new_session=True,
    )
    deadline = time.monotonic() + min(max(request.get("timeout_ms", 15000), 100), 120000) / 1000
    captured = {"stdout": bytearray(), "stderr": bytearray()}
    selector = selectors.DefaultSelector()
    selector.register(process.stdout, selectors.EVENT_READ, "stdout")
    selector.register(process.stderr, selectors.EVENT_READ, "stderr")
    try:
        while selector.get_map():
            if time.monotonic() >= deadline:
                raise TimeoutError("Command exceeded deadline")
            for key, _ in selector.select(timeout=0.1):
                chunk = os.read(key.fileobj.fileno(), 65536)
                if not chunk:
                    selector.unregister(key.fileobj)
                else:
                    output = captured[key.data]
                    output.extend(chunk[:max(0, MAX_OUTPUT - len(output))])
        code = process.wait(timeout=max(0.1, deadline - time.monotonic()))
    finally:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.wait()
        selector.close()
    return {"version": 1, "exit_code": code, "artifacts": export_artifacts(root), **{k: bytes(v).decode("utf-8", errors="replace") for k, v in captured.items()}}


def main():
    with open("/dev/virtio-ports/org.grokmuse.agent", "r+b", buffering=0) as port:
        port.write(b"GROK_MUSE_GUEST_V1\n")
        raw = port.readline(16 * 1024 * 1024 + 1)
        if len(raw) > 16 * 1024 * 1024:
            raise ValueError("Request too large")
        try:
            response = run(json.loads(raw))
        except Exception as error:
            response = {"version": 1, "exit_code": 1, "stdout": "", "stderr": str(error)}
        port.write(json.dumps(response).encode() + b"\n")


if __name__ == "__main__":
    main()
