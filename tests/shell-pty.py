#!/usr/bin/env python3
"""Exercise the Boreal shell's terminal interactions, confirmation prompts, and secret safety."""

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
        tmp_path = pathlib.Path(tmp)
        marker = tmp_path / "must-not-exist"
        base_env = {
            k: v
            for k, v in os.environ.items()
            if not k.startswith("SSH_")
        }
        env = {
            **base_env,
            "TERM": "xterm-256color",
            "HOME": tmp,
            "BOREAL_TEST_HOME": tmp,
        }

        # Unsupported SSH -c commands and inline secret args must fail closed.
        for bad_cmd in ["/bin/sh", "sftp-server", "workers", "secrets set tok inline"]:
            res = subprocess.run(
                [*sys.argv[1:], "-c", bad_cmd],
                input=b"data",
                capture_output=True,
                env=env,
            )
            assert res.returncode != 0, f"expected failure for -c {bad_cmd!r}"

        # Non-interactive stdin secret upload writes 0700 dir + 0600 file without echoing value.
        piped = subprocess.run(
            [*sys.argv[1:], "-c", "secrets set piped_item"],
            input=b"non-echoed-test-payload",
            capture_output=True,
            env=env,
            check=True,
        )
        assert b"non-echoed-test-payload" not in piped.stdout
        assert b"non-echoed-test-payload" not in piped.stderr
        secrets_dir = tmp_path / ".config/boreal/secrets"
        piped_file = secrets_dir / "piped_item"
        assert (secrets_dir.stat().st_mode & 0o777) == 0o700
        assert (piped_file.stat().st_mode & 0o777) == 0o600

        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
        process = subprocess.Popen(sys.argv[1:], stdin=slave, stdout=slave, stderr=slave, env=env)
        os.close(slave)

        def wait_for(*needles):
            data = bytearray()
            deadline = time.monotonic() + 10
            cursor_replies = 0
            while not all(needle in data for needle in needles):
                timeout = deadline - time.monotonic()
                if timeout <= 0 or not select.select([master], [], [], timeout)[0]:
                    raise AssertionError(f"missing {needles!r} in {bytes(data)!r}")
                try:
                    chunk = os.read(master, 8192)
                except OSError as err:
                    raise AssertionError(f"shell closed before {needles!r}: {bytes(data)!r}") from err
                if not chunk:
                    raise AssertionError(f"shell closed before {needles!r}: {bytes(data)!r}") from err
                data.extend(chunk)
                # Crossterm asks the terminal for its cursor position; the PTY
                # is not a terminal emulator, so answer the query ourselves.
                requests = data.count(b"\x1b[6n")
                for _ in range(requests - cursor_replies):
                    os.write(master, b"\x1b[1;1R")
                cursor_replies = requests
            return bytes(data)

        try:
            wait_for(b"mock-only", b"boreal\x1b", b"\x1b[6 q")

            # Partial commands are underlined; the selected match uses the
            # terminal defaults in reverse, while inactive text is bright black.
            os.write(master, b"st")
            wait_for(b"\x1b[4mst", b"\x1b[6 q")
            os.write(master, b"\t")
            wait_for(
                b"\x1b[4;7;49;39mst",
                b"\x1b[7;49;39mart",
                b"\x1b[7;49;39m   Start a workflow",
                b"\x1b[4;90mst",
                b"\x1b[90matus",
                b"\x1b[33mDescribe a workflow",
            )
            # Selection follows Tab without coloring the selected description's
            # background; inactive descriptions retain their original yellow.
            os.write(master, b"\t")
            wait_for(
                b"\x1b[7;49;39m  Describe a workflow",
                b"\x1b[33mStart a workflow",
            )
            os.write(master, b"\x03")
            wait_for(b"boreal\x1b", b"\x1b[6 q")

            # Unknown commands are red and recognized commands are green.
            os.write(master, b"unknown")
            wait_for(b"\x1b[31munknown", b"\x1b[6 q")
            os.write(master, b"\x03")
            wait_for(b"boreal\x1b", b"\x1b[6 q")
            os.write(master, b"workers")
            wait_for(b"\x1b[32mworkers", b"\x1b[6 q")
            os.write(master, b"\x03")
            wait_for(b"boreal\x1b", b"\x1b[6 q")

            # Options include their dashes, but not the value after '='.
            os.write(master, b"list --owner=bob")
            wait_for(b"\x1b[33m--owner", b"=bob", b"\x1b[6 q")
            os.write(master, b"\x03")
            wait_for(b"boreal\x1b", b"\x1b[6 q")

            # Single-candidate Tab immediately applies completion without opening the menu.
            os.write(master, b"work")
            wait_for(b"\x1b[4mwork", b"\x1b[6 q")
            os.write(master, b"\t")
            single_tab = wait_for(b"\x1b[32mworkers\x1b[0m ", b"\x1b[6 q")
            assert b"List available workers" not in single_tab
            os.write(master, b"\r")
            wait_for(b"[mock] alice/greet (DemoWorkflow)")

            # The previous command is available through Up and as a light-black right-arrow hint.
            os.write(master, b"\x1b[A\r")
            wait_for(b"[mock] alice/greet (DemoWorkflow)")
            os.write(master, b"wo")
            wait_for(b"\x1b[90mrkers")
            os.write(master, b"\x1b[C\r")
            wait_for(b"[mock] alice/greet (DemoWorkflow)")

            # Even shell metacharacters passed as an argument have no side effects.
            line = f"start greet test '$(touch {marker})' 5\r".encode()
            os.write(master, line)
            wait_for(b"[mock] started DemoWorkflow 'test'")
            assert not marker.exists(), "a shell metacharacter was executed"

            # Cancel requires confirmation: 'n' aborts, 'y' proceeds.
            os.write(master, b"cancel run-1 --owner bob\r")
            wait_for(b"Cancel DemoWorkflow 'run-1'", b"[y/N]: ")
            os.write(master, b"n\r")
            wait_for(b"cancellation aborted.")

            os.write(master, b"cancel run-1 --owner bob\r")
            wait_for(b"Cancel DemoWorkflow 'run-1'", b"[y/N]: ")
            os.write(master, b"y\r")
            wait_for(b"[mock] cancellation requested for DemoWorkflow 'run-1'")

            # Interactive secret entry disables echo and never prints secret contents.
            os.write(master, b"secrets set tty_item\r")
            wait_for(b"input hidden): ")
            os.write(master, b"quiet-tty-value\n")
            secret_out = wait_for(b"Stored secret 'tty_item' (15 bytes).")
            assert b"quiet-tty-value" not in secret_out, "secret value was echoed to terminal"
            assert ((secrets_dir / "tty_item").stat().st_mode & 0o777) == 0o600

            os.write(master, b"secrets list\r")
            list_out = wait_for(b"piped_item (23 bytes, mode 0600)", b"tty_item (15 bytes, mode 0600)")
            assert b"quiet-tty-value" not in list_out

            os.write(master, b"secrets remove tty_item\r")
            wait_for(b"Remove secret 'tty_item'? [y/N]: ")
            os.write(master, b"y\r")
            wait_for(b"Removed secret 'tty_item'.")
            assert not (secrets_dir / "tty_item").exists()

            os.write(master, b"exit\r")
            data = bytearray()
            replies = 0
            deadline = time.monotonic() + 10
            while True:
                timeout = deadline - time.monotonic()
                if timeout <= 0:
                    raise AssertionError(f"shell did not exit: {bytes(data)!r}")
                if not select.select([master], [], [], timeout)[0]:
                    if process.poll() is not None:
                        break
                    continue
                try:
                    chunk = os.read(master, 8192)
                except OSError:
                    break
                if not chunk:
                    break
                data.extend(chunk)
                requests = data.count(b"\x1b[6n")
                for _ in range(requests - replies):
                    os.write(master, b"\x1b[1;1R")
                replies = requests
            assert process.wait(timeout=5) == 0, bytes(data)
            assert b"\x1b[0 q" in data, "terminal cursor was not reset on exit"
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
            os.close(master)

        def run_rc_pty_session(rc_contents: str, steps):
            rc_file = tmp_path / ".borealrc"
            rc_file.write_text(rc_contents, encoding="utf-8")
            rc_file.chmod(0o600)

            rc_master, rc_slave = pty.openpty()
            fcntl.ioctl(rc_slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
            proc = subprocess.Popen(
                sys.argv[1:], stdin=rc_slave, stdout=rc_slave, stderr=rc_slave, env=env
            )
            os.close(rc_slave)

            def expect(*needles):
                buf = bytearray()
                limit = time.monotonic() + 10
                replies = 0
                while not all(n in buf for n in needles):
                    rem = limit - time.monotonic()
                    if rem <= 0 or not select.select([rc_master], [], [], rem)[0]:
                        raise AssertionError(f"missing {needles!r} in {bytes(buf)!r}")
                    try:
                        part = os.read(rc_master, 8192)
                    except OSError as err:
                        raise AssertionError(f"closed before {needles!r}: {bytes(buf)!r}") from err
                    if not part:
                        raise AssertionError(f"closed before {needles!r}: {bytes(buf)!r}")
                    buf.extend(part)
                    reqs = buf.count(b"\x1b[6n")
                    for _ in range(reqs - replies):
                        os.write(rc_master, b"\x1b[1;1R")
                    replies = reqs
                return bytes(buf)

            try:
                steps(rc_master, expect)
                os.write(rc_master, b"exit\r")
                tail = bytearray()
                replies = 0
                limit = time.monotonic() + 10
                while True:
                    rem = limit - time.monotonic()
                    if rem <= 0:
                        raise AssertionError(f"shell did not exit: {bytes(tail)!r}")
                    if not select.select([rc_master], [], [], rem)[0]:
                        if proc.poll() is not None:
                            break
                        continue
                    try:
                        part = os.read(rc_master, 8192)
                    except OSError:
                        break
                    if not part:
                        break
                    tail.extend(part)
                    reqs = tail.count(b"\x1b[6n")
                    for _ in range(reqs - replies):
                        os.write(rc_master, b"\x1b[1;1R")
                    replies = reqs
                assert proc.wait(timeout=5) == 0, bytes(tail)
                assert b"\x1b[0 q" in tail
            finally:
                if proc.poll() is None:
                    proc.terminate()
                    proc.wait(timeout=5)
                os.close(rc_master)

        # ~/.borealrc edit_mode: vi (bar cursor in insert, block cursor in normal, 0dw + A<Tab>).
        def verify_vi(fd, expect):
            expect(b"mock-only", b": ", b"\x1b[6 q")
            os.write(fd, b"bad work")
            expect(b"work")
            os.write(fd, b"\x1b")
            expect(b"\x1b[2 q")
            os.write(fd, b"0dwA")
            expect(b"\x1b[6 q")
            os.write(fd, b"\t")
            expect(b"\x1b[32mworkers\x1b[0m ")
            os.write(fd, b"\x1b")
            expect(b"\x1b[2 q")
            os.write(fd, b"\r")
            expect(b"[mock] alice/greet (DemoWorkflow)")

        run_rc_pty_session("edit_mode: vi\n", verify_vi)

        # ~/.borealrc edit_mode: helix (selection-first gh + w + d, gl + a<Tab>, normal Enter).
        def verify_helix(fd, expect):
            expect(b"mock-only", b": ", b"\x1b[6 q")
            os.write(fd, b"bad work")
            expect(b"work")
            os.write(fd, b"\x1b")
            expect(b"\x1b[2 q")
            os.write(fd, b"ghwdgla")
            expect(b"\x1b[6 q")
            os.write(fd, b"\t")
            expect(b"\x1b[32mworkers\x1b[0m ")
            os.write(fd, b"\x1b")
            expect(b"\x1b[2 q")
            os.write(fd, b"\r")
            expect(b"[mock] alice/greet (DemoWorkflow)")

        run_rc_pty_session("edit_mode: helix\n", verify_helix)

        # Invalid ~/.borealrc warns and falls back safely to emacs mode without locking out login.
        def verify_fallback(fd, expect):
            expect(b"warning: invalid ~/.borealrc", b"falling back to emacs mode", b"\x1b[6 q")

        run_rc_pty_session("edit_mode: unsupported\n", verify_fallback)

    print("PTY styles, cursor, edit modes, completion, history, confirmation, and secrets: passed")


if __name__ == "__main__":
    main()
