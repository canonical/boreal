#!/usr/bin/env python3
"""Exercise the Boreal shell's terminal interactions, confirmation prompts, and secret safety."""

import fcntl
import json
import os
import pathlib
import pty
import select
import socket
import struct
import subprocess
import sys
import tempfile
import termios
import threading
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
                    raise AssertionError(f"shell closed before {needles!r}: {bytes(data)!r}")
                data.extend(chunk)
                # Crossterm asks the terminal for its cursor position; the PTY
                # is not a terminal emulator, so answer the query ourselves.
                requests = data.count(b"\x1b[6n")
                for _ in range(requests - cursor_replies):
                    os.write(master, b"\x1b[1;1R")
                cursor_replies = requests
            return bytes(data)

        ok_arrow = "\x1b[1;32m❯\x1b[0m ".encode()
        err_arrow = "\x1b[1;31m❯\x1b[0m ".encode()
        norm_arrow = "\x1b[1;36m❮\x1b[0m ".encode()
        sel_arrow = "\x1b[1;35m❮\x1b[0m ".encode()
        drain_badge = "\x1b[31m󰞴\x1b[0m ".encode()

        try:
            wait_for(
                b"mock-only",
                b"\x1b[22;0t\x1b]0;Boreal alice@mock\x07",
                "\x1b[36malice\x1b[0m on \x1b[35m󰇅 mock\x1b[0m".encode(),
                "\x1b[90m[\x1b[0m\x1b[92m 1\x1b[0m\x1b[90m]\x1b[0m".encode(),
                " is \u2744\ufe0f \x1b[36munknown\x1b[0m".encode(),
                ok_arrow,
                b"\x1b[6 q",
            )

            # Partial commands are underlined; the selected match uses the
            # terminal defaults in reverse, while inactive text is bright black.
            os.write(master, b"r")
            wait_for(b"\x1b[4mr", b"\x1b[6 q")
            os.write(master, b"\t")
            wait_for(
                b"\x1b[4;7;49;39mr",
                b"\x1b[7;49;39mun",
                b"\x1b[7;49;39m     Run a workflow",
                b"\x1b[4;90mr",
                b"\x1b[90mesult",
                b"\x1b[33mShow a workflow result",
            )
            # Selection follows Tab without coloring the selected description's
            # background; inactive descriptions retain their original yellow.
            os.write(master, b"\t")
            wait_for(
                b"\x1b[7;49;39m  Show a workflow result",
                b"\x1b[33mRun a workflow",
            )
            os.write(master, b"\x03")
            wait_for(ok_arrow, b"\x1b[6 q")

            # Unknown commands are red and recognized commands are green.
            os.write(master, b"unknown")
            wait_for(b"\x1b[31munknown", b"\x1b[6 q")
            os.write(master, b"\x03")
            wait_for(ok_arrow, b"\x1b[6 q")
            os.write(master, b"workers")
            wait_for(b"\x1b[32mworkers", b"\x1b[6 q")
            os.write(master, b"\x03")
            wait_for(ok_arrow, b"\x1b[6 q")

            # Submitting an invalid command turns the prompt indicator red;
            # `help` succeeds and restores the green indicator.
            os.write(master, b"badcmd\r")
            wait_for(b"unrecognized subcommand", err_arrow, b"\x1b[6 q")
            os.write(master, b"help\r")
            wait_for(b"Boreal lab shell", ok_arrow, b"\x1b[6 q")
            # Clear 'badcmd' / 'help' from top of history by running workers below.

            # Options include their dashes, but not the value after '='.
            os.write(master, b"list --owner=bob")
            wait_for(b"\x1b[33m--owner", b"=bob", b"\x1b[6 q")
            os.write(master, b"\x03")
            wait_for(ok_arrow, b"\x1b[6 q")

            # Single-candidate Tab immediately applies completion without opening the menu.
            os.write(master, b"work")
            wait_for(b"\x1b[4mwork", b"\x1b[6 q")
            os.write(master, b"\t")
            single_tab = wait_for(b"\x1b[32mworkers\x1b[0m ", b"\x1b[6 q")
            assert b"List available workers" not in single_tab
            os.write(master, b"\r")
            wait_for(b"[mock] alice/greet (DemoWorkflow)", ok_arrow)

            # The previous command is available through Up and as a light-black right-arrow hint.
            os.write(master, b"\x1b[A\r")
            wait_for(b"[mock] alice/greet (DemoWorkflow)", ok_arrow)
            os.write(master, b"wo")
            wait_for(b"\x1b[90mrkers")
            os.write(master, b"\x1b[C\r")
            wait_for(b"[mock] alice/greet (DemoWorkflow)", ok_arrow)

            # Even shell metacharacters passed as an argument have no side effects;
            # starting a workflow updates the prompt status bracket to '[ 2]'.
            payload = json.dumps({"name": f"$(touch {marker})", "wait_seconds": 5})
            line = f"run greet --id test --input-json '{payload}'\r".encode()
            os.write(master, line)
            wait_for(
                b"[mock] started DemoWorkflow 'test'",
                "\x1b[90m[\x1b[0m\x1b[92m 2\x1b[0m\x1b[90m]\x1b[0m".encode(),
                ok_arrow,
            )
            assert not marker.exists(), "a shell metacharacter was executed"

            # 'v' + Tab immediately completes 'view '; running 'view test' enters the
            # alternate-screen live TUI and 'q' cleanly restores the prompt.
            os.write(master, b"v\t")
            wait_for(b"\x1b[32mview\x1b[0m ", b"\x1b[6 q")
            os.write(master, b"test\r")
            wait_for(
                b"\x1b[?1049h",
                b"DemoWorkflow",
                b"'test'",
                b"STEPS & IO",
                b"LIVE EVENT LOG",
                b"WorkflowStarted",
            )
            os.write(master, b"q")
            wait_for(b"\x1b[?1049l", ok_arrow, b"\x1b[6 q")

            # Cancel requires confirmation: 'n' aborts, 'y' proceeds.
            os.write(master, b"cancel run-1 --owner bob\r")
            wait_for(b"Cancel DemoWorkflow 'run-1'", b"[y/N]: ")
            os.write(master, b"n\r")
            wait_for(b"cancellation aborted.", ok_arrow)

            os.write(master, b"cancel run-1 --owner bob\r")
            wait_for(b"Cancel DemoWorkflow 'run-1'", b"[y/N]: ")
            os.write(master, b"y\r")
            wait_for(b"[mock] cancellation requested for DemoWorkflow 'run-1'", ok_arrow)

            # Cancelling both of alice's workflows drops the count to 0 and omits '[...]'.
            os.write(master, b"cancel sample-1\r")
            wait_for(b"Cancel DemoWorkflow 'sample-1'", b"[y/N]: ")
            os.write(master, b"y\r")
            wait_for(
                b"[mock] cancellation requested for DemoWorkflow 'sample-1'",
                "\x1b[90m[\x1b[0m\x1b[92m 1\x1b[0m\x1b[90m]\x1b[0m".encode(),
                ok_arrow,
            )

            os.write(master, b"cancel test\r")
            wait_for(b"Cancel DemoWorkflow 'test'", b"[y/N]: ")
            os.write(master, b"y\r")
            zero_out = wait_for(
                b"[mock] cancellation requested for DemoWorkflow 'test'",
                "\x1b[35m󰇅 mock\x1b[0m is \u2744\ufe0f \x1b[36munknown\x1b[0m".encode(),
                ok_arrow,
            )
            assert "".encode() not in zero_out.split(b"cancellation requested")[-1]

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

            # Recall the editable, quoted line exactly, and submit it a second time.
            raw_run = b"run greet --input-json '{ \"name\": \"Alice\", \"wait_seconds\": 5 }'"
            os.write(master, raw_run + b"\r")
            wait_for(b"[mock] started DemoWorkflow", ok_arrow)
            os.write(master, b"\x1b[A")
            wait_for(b"greet ", b"--input-json", b"'{ \"name\": \"Alice\", \"wait_seconds\": 5 }'")
            os.write(master, b"\r")
            wait_for(b"[mock] started DemoWorkflow", ok_arrow)
            disk_history = (tmp_path / "boreal.history").read_bytes()
            assert disk_history.splitlines()[-1] == b"run greet"
            assert b"--input-json" not in disk_history

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
            assert b"\x1b]0;\x07\x1b[23;0t" in data, "terminal title was not restored on exit"
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

        def verify_reconnect_and_picker(fd, expect):
            expect(ok_arrow)
            os.write(fd, b"\x1b[A")
            recalled = expect(b"greet", b"\x1b[6 q")
            assert b"--input-json" not in recalled
            os.write(fd, b"\x03")
            expect(ok_arrow)
            for i in range(1, 8):
                os.write(fd, f"run greet --id pager-{i} --input-json '{{\"name\":\"A\"}}'\r".encode())
                expect(f"[mock] started DemoWorkflow 'pager-{i}'".encode(), ok_arrow)

            os.write(fd, b"status\r")
            expect(b"Page 1/2", "› pager-1".encode(), b"pager-5")
            os.write(fd, b"\x1b[B" * 3)
            expect("› pager-4".encode())
            os.write(fd, b"\x1b[C")
            expect(b"Page 2/2", "› sample-1".encode())
            os.write(fd, b"\x1b[D")
            expect(b"Page 1/2", "› pager-4".encode())
            os.write(fd, b"\r")
            expect(b"pager-4", ok_arrow)

        run_rc_pty_session("edit_mode: emacs\n", verify_reconnect_and_picker)

        # ~/.borealrc edit_mode: vi (bar cursor + ❯ in insert, block cursor + ❮ in normal).
        def verify_vi(fd, expect):
            expect(b"mock-only", ok_arrow, b"\x1b[6 q")
            os.write(fd, b"bad work")
            expect(b"work")
            os.write(fd, b"\x1b")
            expect(norm_arrow, b"\x1b[2 q")
            os.write(fd, b"0dwA")
            expect(ok_arrow, b"\x1b[6 q")
            os.write(fd, b"\t")
            expect(b"\x1b[32mworkers\x1b[0m ")
            os.write(fd, b"\x1b")
            expect(norm_arrow, b"\x1b[2 q")
            os.write(fd, b"\r")
            expect(b"[mock] alice/greet (DemoWorkflow)", ok_arrow)

        run_rc_pty_session("edit_mode: vi\n", verify_vi)

        # ~/.borealrc edit_mode: helix (❯ insert, cyan ❮ normal, purple ❮ select, normal Enter).
        def verify_helix(fd, expect):
            expect(b"mock-only", ok_arrow, b"\x1b[6 q")
            os.write(fd, b"bad work")
            expect(b"work")
            os.write(fd, b"\x1b")
            expect(norm_arrow, b"\x1b[2 q")
            os.write(fd, b"v")
            expect(sel_arrow)
            os.write(fd, b"\x1b")
            expect(norm_arrow)
            os.write(fd, b"ghwdgla")
            expect(ok_arrow, b"\x1b[6 q")
            os.write(fd, b"\t")
            expect(b"\x1b[32mworkers\x1b[0m ")
            os.write(fd, b"\x1b")
            expect(norm_arrow, b"\x1b[2 q")
            os.write(fd, b"\r")
            expect(b"[mock] alice/greet (DemoWorkflow)", ok_arrow)

        run_rc_pty_session("edit_mode: helix\n", verify_helix)

        # Invalid ~/.borealrc warns and falls back safely to emacs mode without locking out login.
        def verify_fallback(_, expect):
            expect(b"warning: invalid ~/.borealrc", b"falling back to emacs mode", ok_arrow, b"\x1b[6 q")

        run_rc_pty_session("edit_mode: unsupported\n", verify_fallback)

        # Standalone mock-broker harness (live mode without VM): verify alice@demo-lab,
        # live running count + draining badge, and silent fallback when broker is offline.
        etc_dir = tmp_path / "etc-boreal"
        (etc_dir / "users" / "alice").mkdir(parents=True)
        etc_dir.chmod(0o700)
        (etc_dir / "users").chmod(0o700)
        (etc_dir / "users" / "alice").chmod(0o700)
        version_file = etc_dir / "version"
        version_file.write_text("0.2.0\n", encoding="utf-8")
        version_file.chmod(0o400)
        schema_dir = tmp_path / "schema"
        schema_dir.mkdir()
        schema_dir.chmod(0o755)
        schema_yaml = schema_dir / "schema.yaml"
        schema_yaml.write_text(
            "greet:\n  workflow: DemoWorkflow\n  args:\n  - name: name\n    type: string\n",
            encoding="utf-8",
        )
        schema_yaml.chmod(0o444)
        schemas_yaml = etc_dir / "schemas.yaml"
        schemas_yaml.write_text(
            f"directories:\n  - {schema_dir}\n",
            encoding="utf-8",
        )
        schemas_yaml.chmod(0o400)
        perms_yaml = etc_dir / "users" / "alice" / "perms.yaml"
        perms_yaml.write_text(
            "user: alice\npermissions: {read: false, write: false}\nworkers:\n  greet:\n    package: greet\n    permissions: {read: false, write: false}\n",
            encoding="utf-8",
        )
        perms_yaml.chmod(0o400)

        try:
            (tmp_path / ".borealrc").unlink(missing_ok=True)
            sock_path = tmp_path / "broker.sock"
            live_env = {
                **env,
                "BOREAL_TEST_ETC_DIR": str(etc_dir),
                "BOREAL_TEST_CALLER": "alice",
                "BOREAL_TEST_HOSTNAME": "demo-lab",
                "BOREAL_TEST_BROKER_SOCKET": str(sock_path),
            }

            # 1) No broker status or deployed revision; the legacy version is not shown.
            orig_env = env
            env = live_env
            def verify_offline_broker(_, expect):
                out = expect(
                    b"Boreal lab shell (alice)",
                    b"\x1b[22;0t\x1b]0;Boreal alice@demo-lab\x07",
                    "\x1b[36malice\x1b[0m on \x1b[35m󰇅 demo-lab\x1b[0m is \u2744\ufe0f \x1b[36munknown\x1b[0m".encode(),
                    ok_arrow,
                )
                assert "".encode() not in out
                assert "".encode() not in out
                assert "󰞴".encode() not in out
                assert b"connect broker" not in out

            run_rc_pty_session("edit_mode: emacs\n", verify_offline_broker)

            # 2) Mock Unix-socket broker serves Request::Summary (tag 9) -> [ 1  2] + line-2 󰞴.
            stop_server = threading.Event()
            srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            srv.bind(str(sock_path))
            srv.listen(4)
            srv.settimeout(0.2)

            def serve_summary():
                while not stop_server.is_set():
                    try:
                        conn, _ = srv.accept()
                    except TimeoutError:
                        continue
                    except OSError:
                        break
                    with conn:
                        hdr = conn.recv(4)
                        if len(hdr) == 4:
                            (plen,) = struct.unpack(">I", hdr)
                            payload = conn.recv(plen)
                            if payload == b"\x09":
                                # Response::Summary(PromptSummary { workflows: Some({ paused: 1, running: 2 }), draining: true })
                                body = b"\x0a\x01\x00\x00\x00\x01\x00\x00\x00\x02\x01"
                                conn.sendall(struct.pack(">I", len(body)) + body)

            thread = threading.Thread(target=serve_summary, daemon=True)
            thread.start()
            try:
                def verify_live_summary(_, expect):
                    expect(
                        "\x1b[36malice\x1b[0m on \x1b[35m󰇅 demo-lab\x1b[0m".encode(),
                        "\x1b[90m[\x1b[0m\x1b[94m 1\x1b[0m \x1b[92m 2\x1b[0m\x1b[90m]\x1b[0m".encode(),
                        " is \u2744\ufe0f \x1b[36munknown\x1b[0m".encode(),
                        b"\r\n" + drain_badge + b"\x1b[39m" + ok_arrow,
                    )

                run_rc_pty_session("edit_mode: emacs\n", verify_live_summary)
            finally:
                stop_server.set()
                srv.close()
                thread.join(timeout=2)
                env = orig_env
        finally:
            version_file.chmod(0o600)
            schemas_yaml.chmod(0o600)
            schema_yaml.chmod(0o600)
            perms_yaml.chmod(0o600)

    print("PTY Starship prompt, styles, cursor, edit modes, completion, history, confirmation, and secrets: passed")


if __name__ == "__main__":
    main()
