#!/usr/bin/env python3
"""Run the local lab with fixture data under a throwaway profile and checkout."""

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


def interactive(command, cwd, env, steps):
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 0, 0))
    child = subprocess.Popen(command, cwd=cwd, env=env, stdin=slave, stdout=slave, stderr=slave)
    os.close(slave)
    try:
        for keys, expected in steps:
            if keys:
                os.write(master, keys)
            output = bytearray()
            queries = 0
            deadline = time.monotonic() + 60
            while not all(needle in output for needle in expected):
                remaining = deadline - time.monotonic()
                assert remaining > 0 and select.select([master], [], [], remaining)[0], output
                try:
                    chunk = os.read(master, 8192)
                except OSError as err:
                    raise AssertionError(f"shell closed: {output!r}") from err
                assert chunk, f"shell closed: {output!r}"
                output.extend(chunk)
                count = output.count(b"\x1b[6n")
                for _ in range(count - queries):
                    os.write(master, b"\x1b[1;1R")
                queries = count
            assert b"nineteen-byte-token" not in output
        os.write(master, b"exit\r")
        assert child.wait(timeout=10) == 0
    finally:
        if child.poll() is None:
            child.kill()
            child.wait()
        os.close(master)


def smoke(first, rebuilt, root):
    checkout = root / "checkout"
    checkout.mkdir()
    state = root / "state"
    env = {
        **{k: v for k, v in os.environ.items() if not k.startswith("SSH_")},
        "XDG_STATE_HOME": str(state),
        "TERM": "xterm-256color",
        "TEMPORAL_ADDRESS": "192.0.2.1:7233",
        "TEMPORAL_NAMESPACE": "not-local",
        "BOREAL_UPSTREAM_TEMPORAL_ADDR": "192.0.2.1:7233",
    }

    def cli(binary, *args, data=None):
        command = [binary, *args]
        result = subprocess.run(command, cwd=checkout, env=env, input=data,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=100)
        assert result.returncode == 0, (command, result.stdout, result.stderr)
        return result.stdout

    def vault_result(binary, workflow_id):
        for _ in range(50):
            result = cli(binary, "result", workflow_id)
            if b"Central vault verified the token." in result:
                return
            time.sleep(.2)
        raise AssertionError(f"missing vault result: {result!r}")

    ready = "\x1b[1;32m❯\x1b[0m ".encode()
    try:
        interactive([first], checkout, env, [
            (b"", (b"Boreal local shell", ready)),
            (b"secrets set api_tok\r", (b"input hidden): ",)),
            (b"nineteen-byte-token\n", (b"Stored secret 'api_tok'", ready)),
            (b"run greet --id interactive\r", (b"Recipient name",)),
            (b"Form-only\r", (b"Timer duration",)),
            (b"1\r", (b"Enter to start",)),
            (b"\r", (b"started", b"interactive", ready)),
            (b"run greet --id vault-first --input-json '{\"name\":\"vault-check\",\"wait_seconds\":1}'\r",
             (b"started", b"vault-first", ready)),
        ])
        vault_result(first, "vault-first")
        home, = state.glob("boreal-local/*/home")
        history = home / "boreal.history"
        assert history.stat().st_mode & 0o777 == 0o600
        selectors = history.read_bytes()
        assert selectors.splitlines()[-1] == b"run greet --id vault-first"
        assert b"Form-only" not in selectors and b"vault-check" not in selectors
        assert b"--input-json" not in selectors
        secrets = home / ".config/boreal/secrets"
        assert secrets.stat().st_mode & 0o777 == 0o700
        assert (secrets / "api_tok").stat().st_mode & 0o777 == 0o600

        cli(first, "stop")
        # A rebuilt launcher in the same checkout must find the same state.
        interactive([rebuilt], checkout, env, [
            (b"", (b"Boreal local shell", ready)),
            (b"\x1b[A", (b"vault-first",)),
            (b"\x03", (ready,)),
        ])
        vault_result(rebuilt, "vault-first")
        cli(rebuilt, "run", "greet", "--id", "vault-next", "--input-json", "-",
            data=b'{"name":"vault-check","wait_seconds":1}')
        vault_result(rebuilt, "vault-next")
        assert b"vault-next" not in history.read_bytes()
    finally:
        cli(rebuilt, "stop")


if __name__ == "__main__":
    if not 2 <= len(sys.argv) <= 3:
        sys.exit("usage: python3 tests/local-smoke.py BOREAL-LOCAL [REBUILT-BOREAL-LOCAL]")
    first = pathlib.Path(sys.argv[1]).resolve()
    rebuilt = pathlib.Path(sys.argv[-1]).resolve()
    preferred = pathlib.Path("/tmp/opencode")
    with tempfile.TemporaryDirectory(prefix="boreal-local-test-",
                                     dir=preferred if preferred.is_dir() else None) as tmp:
        smoke(first, rebuilt, pathlib.Path(tmp))
    print("local lab smoke test passed")
