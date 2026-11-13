#!/usr/bin/env python3
"""Exercise the local mock shell's terminal interactions with a disposable PTY."""

import fcntl
import os
import pathlib
import pty
import select
import struct
import subprocess
import sys
import tempfile
import termios
import time


def main():
    if len(sys.argv) < 2:
        raise SystemExit("usage: python3 tests/shell-pty.py SHELL_COMMAND [ARG ...]")

    preferred_tmp = pathlib.Path("/tmp/opencode")
    tmp_dir = preferred_tmp if preferred_tmp.is_dir() else None
    with tempfile.TemporaryDirectory(prefix="boreal-shell-test-", dir=tmp_dir) as tmp:
        marker = pathlib.Path(tmp) / "must-not-exist"
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
        env = {**os.environ, "TERM": "xterm-256color", "HOME": tmp}
        process = subprocess.Popen(sys.argv[1:], stdin=slave, stdout=slave, stderr=slave, env=env)
        os.close(slave)

        def wait_for(needle):
            data = bytearray()
            deadline = time.monotonic() + 10
            cursor_replies = 0
            while needle not in data:
                timeout = deadline - time.monotonic()
                if timeout <= 0 or not select.select([master], [], [], timeout)[0]:
                    raise AssertionError(f"missing {needle!r} in {bytes(data)!r}")
                try:
                    chunk = os.read(master, 8192)
                except OSError as err:
                    raise AssertionError(f"shell closed before {needle!r}: {bytes(data)!r}") from err
                if not chunk:
                    raise AssertionError(f"shell closed before {needle!r}: {bytes(data)!r}")
                data.extend(chunk)
                # Crossterm asks the terminal for its cursor position; the PTY
                # is not a terminal emulator, so answer the query ourselves.
                requests = data.count(b"\x1b[6n")
                for _ in range(requests - cursor_replies):
                    os.write(master, b"\x1b[1;1R")
                cursor_replies = requests
            return bytes(data)

        try:
            assert b"mock-only" in wait_for(b"boreal\x1b")

            # Tab completes a command; Enter handles either menu acceptance or submission.
            os.write(master, b"work")
            wait_for(b"work")
            os.write(master, b"\t")
            wait_for(b"workers")
            os.write(master, b"\r\r")
            wait_for(b"[mock] Workers")

            # The previous command is available through Up and as a right-arrow hint.
            os.write(master, b"\x1b[A\r")
            wait_for(b"[mock] Workers")
            os.write(master, b"wo")
            wait_for(b"rkers")
            os.write(master, b"\x1b[C\r")
            wait_for(b"[mock] Workers")

            # Even shell metacharacters passed as an argument have no side effects.
            line = f"start greet test '$(touch {marker})' 5\r".encode()
            os.write(master, line)
            wait_for(b"no action performed")
            assert not marker.exists(), "a shell metacharacter was executed"

            os.write(master, b"cancel run-1 --owner bob\r")
            wait_for(b"[mock] Cancel")

            os.write(master, b"exit\r")
            data = bytearray()
            replies = 0
            deadline = time.monotonic() + 10
            while process.poll() is None:
                timeout = deadline - time.monotonic()
                if timeout <= 0:
                    raise AssertionError(f"shell did not exit: {bytes(data)!r}")
                if not select.select([master], [], [], timeout)[0]:
                    continue
                try:
                    data.extend(os.read(master, 8192))
                except OSError:
                    break
                requests = data.count(b"\x1b[6n")
                for _ in range(requests - replies):
                    os.write(master, b"\x1b[1;1R")
                replies = requests
            assert process.wait(timeout=5) == 0, bytes(data)
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
            os.close(master)

    print("PTY completion, history, ghost hint, and mock execution: passed")


if __name__ == "__main__":
    main()
