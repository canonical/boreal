#!/usr/bin/env python3
"""End-to-end guest integration suite for Boreal isolation, shell, admin, and updates."""

from __future__ import annotations

import fcntl
import functools
import http.server
import json
import os
import pathlib
import pty
import pwd
import re
import select
import socket
import ssl
import struct
import subprocess
import sys
import termios
import threading
import time

SSH_OPTS = [
    "ssh",
    "-o",
    "BatchMode=yes",
    "-o",
    "StrictHostKeyChecking=no",
    "-o",
    "UserKnownHostsFile=/dev/null",
]


def ssh_cmd(user: str, *remote_args: str) -> list[str]:
    return [*SSH_OPTS, "-i", f"/tmp/test-ssh/{user}", f"{user}@127.0.0.1", *remote_args]


def run_as_user(username: str, py_code: str, *args: str) -> subprocess.CompletedProcess[str]:
    pw = pwd.getpwnam(username)
    return subprocess.run(
        [
            "setpriv",
            f"--reuid={pw.pw_uid}",
            f"--regid={pw.pw_gid}",
            "--init-groups",
            "python3",
            "-",
            username,
            *args,
        ],
        input=py_code,
        capture_output=True,
        text=True,
        env={**os.environ, "HOME": pw.pw_dir, "USER": username},
    )


ANSI_RE = re.compile(rb"\x1b(?:\[[0-9;?]*[ -/]*[@-~]|\][^\x07\x1b]*(?:\x07|\x1b\\)|[78])")


class SshPtySession:
    def __init__(self, user: str) -> None:
        self.master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 100, 0, 0))
        env = {**os.environ, "TERM": "xterm-256color"}
        self.proc = subprocess.Popen(
            [*SSH_OPTS, "-tt", "-i", f"/tmp/test-ssh/{user}", f"{user}@127.0.0.1"],
            stdin=slave,
            stdout=slave,
            stderr=slave,
            env=env,
        )
        os.close(slave)

    def wait_for(
        self, *needles: bytes, timeout: float = 25.0, wait_prompt: bool = True
    ) -> bytes:
        buf = bytearray()
        plain = b""
        deadline = time.monotonic() + timeout
        cursor_replies = 0
        while not (
            all(needle in plain for needle in needles)
            and (not wait_prompt or plain.rstrip().endswith(b"boreal>"))
        ):
            rem = deadline - time.monotonic()
            if rem <= 0 or not select.select([self.master], [], [], rem)[0]:
                raise AssertionError(f"timeout waiting for {needles!r} in {plain!r}")
            chunk = os.read(self.master, 8192)
            if not chunk:
                raise AssertionError(f"EOF before {needles!r}: {plain!r}")
            buf.extend(chunk)
            requests = buf.count(b"\x1b[6n")
            for _ in range(requests - cursor_replies):
                os.write(self.master, b"\x1b[1;1R")
            cursor_replies = requests
            plain = ANSI_RE.sub(b"", bytes(buf))
        return plain

    def send(self, data: bytes) -> None:
        os.write(self.master, data)

    def close(self) -> None:
        try:
            self.send(b"exit\r")
            self.proc.wait(timeout=5)
        except Exception:
            self.proc.kill()
            self.proc.wait()
        os.close(self.master)


def setup_ssh_keys() -> None:
    ssh_dir = pathlib.Path("/tmp/test-ssh")
    auth_dir = pathlib.Path("/run/test-auth-keys")
    ssh_dir.mkdir(mode=0o700, parents=True, exist_ok=True)
    auth_dir.mkdir(mode=0o755, parents=True, exist_ok=True)
    for user in ["alice", "bob", "carol", "admin"]:
        key_file = ssh_dir / user
        if key_file.exists():
            key_file.unlink()
        subprocess.run(
            ["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(key_file)],
            check=True,
        )
        pub_dst = auth_dir / user
        pub_dst.write_bytes(pathlib.Path(f"{key_file}.pub").read_bytes())
        pub_dst.chmod(0o644)


HOST_ISOLATION_PROBE = """
import os, socket, sys

user = sys.argv[1]
for port in [5432, 6933, 6934, 6935, 6939, 7233, 7234, 7235, 7239, 7243]:
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.settimeout(1.0)
    rc = sock.connect_ex(("127.0.0.1", port))
    sock.close()
    assert rc != 0, f"port {port} unexpectedly reachable in host netns for {user}"

for unix_path in ["/run/postgresql/.s.PGSQL.5432", "/run/boreal/updater.sock"]:
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    try:
        sock.connect(unix_path)
        raise AssertionError(f"{unix_path} unexpectedly accepted connection from {user}")
    except PermissionError:
        pass
    finally:
        sock.close()

protected = [
    "/var/lib/postgresql",
    "/var/lib/temporal",
    "/var/lib/boreal-broker",
    "/run/netns/boreal-internal",
    "/etc/boreal/updater.yaml",
]
if user != "bob":
    protected.append("/home/bob")
if user != "alice":
    protected.append("/home/alice")
if user == "admin":
    protected.extend(["/etc/boreal/workers.yaml", "/run/boreal/broker.sock"])

for path in protected:
    try:
        if os.path.isdir(path):
            os.listdir(path)
        else:
            with open(path, "rb"):
                pass
        raise AssertionError(f"{user} unexpectedly read {path}")
    except PermissionError:
        pass
"""


def test_host_and_worker_isolation(temporal_cli: str) -> None:
    print("[vm-suite] 1/4 verifying host & worker isolation boundaries...")
    for user in ["alice", "bob", "carol", "admin"]:
        res = run_as_user(user, HOST_ISOLATION_PROBE)
        assert res.returncode == 0, f"host isolation failed for {user}: {res.stderr}"

    worker_pid = int(
        subprocess.check_output(
            [
                "systemctl",
                "show",
                "-p",
                "MainPID",
                "--value",
                "boreal-worker-alice-greet.service",
            ],
            text=True,
        ).strip()
    )
    assert worker_pid > 1
    ptrace_res = run_as_user(
        "alice",
        f"""
import ctypes
libc = ctypes.CDLL("libc.so.6", use_errno=True)
assert libc.ptrace(16, {worker_pid}, 0, 0) == -1, "same-UID ptrace unexpectedly succeeded"
""",
    )
    assert ptrace_res.returncode == 0, ptrace_res.stderr

    # Inside alice-greet's worker netns:
    # - DescribeNamespace on alice is permitted for worker startup.
    # - Cross-namespace DescribeNamespace on bob MUST fail with PermissionDenied.
    # - Client StartWorkflowExecution / ListWorkflowExecutions MUST fail with PermissionDenied.
    # - Internal Postgres (5432) and History (7234) ports are unreachable.
    pw_alice = pwd.getpwnam("alice")
    alice_env = {**os.environ, "HOME": pw_alice.pw_dir, "USER": "alice"}
    netns_prefix = [
        "ip",
        "netns",
        "exec",
        "boreal-worker-alice-greet",
        "setpriv",
        f"--reuid={pw_alice.pw_uid}",
        f"--regid={pw_alice.pw_gid}",
        "--clear-groups",
    ]
    ok_desc = subprocess.run(
        [
            *netns_prefix,
            temporal_cli,
            "operator",
            "namespace",
            "describe",
            "--address",
            "127.0.0.1:7233",
            "--namespace",
            "alice",
        ],
        capture_output=True,
        text=True,
        env=alice_env,
    )
    assert ok_desc.returncode == 0, f"expected alice namespace describe to pass: {ok_desc.stderr}"

    for denied_args in [
        [
            temporal_cli,
            "operator",
            "namespace",
            "describe",
            "--address",
            "127.0.0.1:7233",
            "--namespace",
            "bob",
        ],
        [
            temporal_cli,
            "workflow",
            "start",
            "--address",
            "127.0.0.1:7233",
            "--namespace",
            "alice",
            "--task-queue",
            "alice-greet",
            "--type",
            "DemoWorkflow",
            "--workflow-id",
            "worker-bypass",
        ],
        [
            temporal_cli,
            "workflow",
            "list",
            "--address",
            "127.0.0.1:7233",
            "--namespace",
            "alice",
        ],
    ]:
        denied = subprocess.run(
            [*netns_prefix, *denied_args],
            capture_output=True,
            text=True,
            env=alice_env,
        )
        assert denied.returncode != 0, f"worker gate failed to block: {denied_args}"
        assert (
            "PermissionDenied" in denied.stderr
            or "denied" in denied.stderr
            or "not permitted" in denied.stderr
            or "mismatch" in denied.stderr
        ), f"unexpected error from worker gate: {denied.stderr}"

    port_check = subprocess.run(
        [
            *netns_prefix,
            "python3",
            "-c",
            "import socket\nfor p in (5432, 7234, 7235):\n s=socket.socket(); assert s.connect_ex(('127.0.0.1', p)) != 0; s.close()\n",
        ],
        capture_output=True,
        text=True,
        env=alice_env,
    )
    assert port_check.returncode == 0, port_check.stderr


def test_ssh_shell_permissions_secrets_and_recovery() -> None:
    print("[vm-suite] 2/4 verifying SSH restrictions, boreal-shell PTY, secrets, & recovery...")
    for user in ["alice", "admin"]:
        for bad_cmd in ["id", "/bin/sh", "workers", "secrets set tok inline_leak"]:
            res = subprocess.run(ssh_cmd(user, bad_cmd), input=b"x", capture_output=True)
            assert res.returncode != 0, f"{user} SSH accepted unsupported command {bad_cmd!r}"

        sftp_res = subprocess.run(
            [
                "sftp",
                "-o",
                "BatchMode=yes",
                "-o",
                "StrictHostKeyChecking=no",
                "-o",
                "UserKnownHostsFile=/dev/null",
                "-i",
                f"/tmp/test-ssh/{user}",
                f"{user}@127.0.0.1",
            ],
            input=b"ls\n",
            capture_output=True,
        )
        assert sftp_res.returncode != 0, f"SFTP unexpectedly succeeded for {user}"

    # Verify AllowTcpForwarding=no blocks local port forwarding.
    fwd = subprocess.Popen(
        [*ssh_cmd("alice"), "-N", "-L", "127.0.0.1:17233:127.0.0.1:22"],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    time.sleep(1.2)
    try:
        sock = socket.create_connection(("127.0.0.1", 17233), timeout=1.5)
        sock.settimeout(1.5)
        banner = sock.recv(64)
        sock.close()
        assert not banner.startswith(b"SSH-"), f"SSH port forwarding bypassed restriction: {banner!r}"
    except OSError:
        pass
    finally:
        fwd.terminate()
        fwd.wait(timeout=5)

    # Stdin secret upload over SSH
    sec_upload = subprocess.run(
        ssh_cmd("alice", "secrets set api_tok"),
        input=b"nineteen-byte-token",
        capture_output=True,
        check=True,
    )
    assert b"Stored secret 'api_tok' (19 bytes)." in sec_upload.stdout
    assert b"nineteen-byte-token" not in sec_upload.stdout + sec_upload.stderr
    assert (os.stat("/home/alice/.config/boreal/secrets").st_mode & 0o777) == 0o700
    assert (os.stat("/home/alice/.config/boreal/secrets/api_tok").st_mode & 0o777) == 0o600

    # Interactive SSH PTY as alice
    alice = SshPtySession("alice")
    try:
        banner = alice.wait_for(b"Boreal lab shell (alice)", b"boreal")
        assert b"mock-only" not in banner

        alice.send(b"permissions ")
        alice.wait_for(b"permissions ", wait_prompt=False)
        alice.send(b"\t")
        comp = alice.wait_for(b"alice", wait_prompt=False)
        assert b"bob" not in comp
        alice.send(b"\x03")
        alice.wait_for(b"boreal>")

        alice.send(b"permissions bob\r")
        alice.wait_for(b"error:")

        alice.send(b"secrets set tty_tok\r")
        alice.wait_for(b"input hidden): ", wait_prompt=False)
        alice.send(b"hidden-tty-val\n")
        tty_out = alice.wait_for(b"Stored secret 'tty_tok' (14 bytes).")
        assert b"hidden-tty-val" not in tty_out
        alice.send(b"secrets list\r")
        alice.wait_for(b"api_tok (19 bytes, mode 0600)", b"tty_tok (14 bytes, mode 0600)")

        # Activity reads ~/.config/boreal/secrets/api_tok without leaking content into Temporal!
        alice.send(b"start greet sec-wf-1 'secret:api_tok' 1\r")
        alice.wait_for(b"started DemoWorkflow 'sec-wf-1' on alice/greet")
        for _ in range(30):
            time.sleep(0.4)
            alice.send(b"result sec-wf-1\r")
            res = alice.wait_for(b"run ID:")
            if b"Secret length verified: 19" in res:
                assert b"nineteen-byte-token" not in res
                break
        else:
            raise AssertionError("sec-wf-1 did not finish")

        # CountWorkflow with UTF-8 multi-byte scalar
        alice.send("start count count-1 'Élan' 1\r".encode())
        alice.wait_for(b"started CountWorkflow 'count-1' on alice/count")
        for _ in range(30):
            time.sleep(0.4)
            alice.send(b"result count-1\r")
            res = alice.wait_for(b"run ID:")
            if b"Character count: 4" in res:
                break
        else:
            raise AssertionError("count-1 did not finish")

        # Workflow recovery across worker restart via restricted admin SSH command
        alice.send(b"start greet rec-wf-1 'Ada' 5\r")
        alice.wait_for(b"started DemoWorkflow 'rec-wf-1' on alice/greet")
        time.sleep(1.0)
        subprocess.run(
            ssh_cmd("admin", "service restart boreal-worker-alice-greet"),
            check=True,
            capture_output=True,
        )
        for _ in range(40):
            time.sleep(0.5)
            alice.send(b"result rec-wf-1\r")
            res = alice.wait_for(b"run ID:")
            if b"Hello, Ada!" in res:
                break
        else:
            raise AssertionError("rec-wf-1 did not recover after worker restart")

        alice.send(b"start count slow-count-1 'Hold' 120\r")
        alice.wait_for(b"started CountWorkflow 'slow-count-1' on alice/count")
    finally:
        alice.close()

    # Interactive SSH PTY as bob: delegated start/read/cancel + denied start/cancel
    bob = SshPtySession("bob")
    try:
        bob.wait_for(b"Boreal lab shell (bob)", b"boreal")
        bob.send(b"permissions alice\r")
        bob.wait_for(b"greet (package lab-worker): read=allowed, write=allowed")

        bob.send(b"start greet bob-del-1 'FromBob' 120 --owner alice\r")
        bob.wait_for(b"started DemoWorkflow 'bob-del-1' on alice/greet")

        bob.send(b"start count bob-denied-1 'No' 5 --owner alice\r")
        bob.wait_for(b"error:")

        bob.send(b"result count-1 --owner alice\r")
        bob.wait_for(b"Character count: 4")

        bob.send(b"cancel slow-count-1 --owner alice\r")
        bob.wait_for(b"error: permission denied")

        bob.send(b"cancel bob-del-1 --owner alice\r")
        bob.wait_for(b"Cancel DemoWorkflow 'bob-del-1'", b"[y/N]: ", wait_prompt=False)
        bob.send(b"n\r")
        bob.wait_for(b"cancellation aborted.")
        bob.send(b"status bob-del-1 --owner alice\r")
        bob.wait_for(b"status:      running")

        bob.send(b"cancel bob-del-1 --owner alice\r")
        bob.wait_for(b"Cancel DemoWorkflow 'bob-del-1'", b"[y/N]: ", wait_prompt=False)
        bob.send(b"y\r")
        bob.wait_for(b"cancellation requested for DemoWorkflow 'bob-del-1'")
    finally:
        bob.close()

    # Interactive SSH PTY as carol: list filtering & denied read on alice/count and bob
    carol = SshPtySession("carol")
    try:
        carol.wait_for(b"Boreal lab shell (carol)", b"boreal>")
        for _ in range(20):
            carol.send(b"list --owner alice\r")
            listing = carol.wait_for(b"list --owner alice")
            if b"rec-wf-1" in listing:
                assert b"count-1" not in listing, f"carol saw private count-1: {listing!r}"
                assert b"slow-count-1" not in listing, f"carol saw slow-count-1: {listing!r}"
                break
            time.sleep(0.5)
        else:
            raise AssertionError("carol did not see public alice/greet workflow in list")

        carol.send(b"status count-1 --owner alice\r")
        carol.wait_for(b"error: permission denied")
        carol.send(b"list --owner bob\r")
        carol.wait_for(b"error:")
    finally:
        carol.close()

    # Cancel slow-count-1 as alice so the host is clean before Suite D
    alice_clean = SshPtySession("alice")
    try:
        alice_clean.wait_for(b"Boreal lab shell (alice)")
        alice_clean.send(b"cancel slow-count-1\r")
        alice_clean.wait_for(b"[y/N]: ", wait_prompt=False)
        alice_clean.send(b"y\r")
        alice_clean.wait_for(b"cancellation requested")
        for _ in range(25):
            alice_clean.send(b"status slow-count-1\r")
            st = alice_clean.wait_for(b"status:")
            if b"cancelled" in st:
                break
            time.sleep(0.3)
    finally:
        alice_clean.close()


def test_restricted_admin_and_bypass_resistance() -> None:
    print("[vm-suite] 3/4 verifying restricted admin interface & bypass resistance...")
    svcs = subprocess.run(ssh_cmd("admin", "services"), capture_output=True, text=True, check=True)
    assert "temporal.service  active=active" in svcs.stdout
    assert "nineteen-byte-token" not in svcs.stdout

    for denied in [
        "service restart sshd",
        "service status postgresql",
        "service status boreal-broker",
        "cat /home/alice/.config/boreal/secrets/api_tok",
        "poweroff",
    ]:
        res = subprocess.run(ssh_cmd("admin", denied), capture_output=True, text=True)
        assert res.returncode != 0, f"admin command unexpectedly succeeded: {denied}"

    pw_admin = pwd.getpwnam("admin")
    for bad_sudo in [
        ["/run/wrappers/bin/sudo", "-n", "/bin/sh", "-c", "id"],
        ["/run/wrappers/bin/sudo", "-n", "cat", "/home/alice/.config/boreal/secrets/api_tok"],
    ]:
        res = subprocess.run(
            [
                "setpriv",
                f"--reuid={pw_admin.pw_uid}",
                f"--regid={pw_admin.pw_gid}",
                "--init-groups",
                *bad_sudo,
            ],
            capture_output=True,
        )
        assert res.returncode != 0

    evil_pager = pathlib.Path("/tmp/evil-pager")
    evil_pager.write_text("#!/bin/sh\ntouch /tmp/admin-env-bypassed\n")
    evil_pager.chmod(0o755)
    ctl_bin = next(
        line.split(":", 1)[1].strip().strip("'\"")
        for line in pathlib.Path("/etc/boreal-admin/policy.yaml").read_text().splitlines()
        if line.startswith("ctlBin:")
    )
    env_res = subprocess.run(
        [
            "setpriv",
            f"--reuid={pw_admin.pw_uid}",
            f"--regid={pw_admin.pw_gid}",
            "--init-groups",
            "env",
            "SYSTEMD_PAGER=/tmp/evil-pager",
            "PAGER=/tmp/evil-pager",
            "PATH=/tmp",
            "/run/wrappers/bin/sudo",
            "-n",
            ctl_bin,
            "services",
        ],
        capture_output=True,
        text=True,
        check=True,
    )
    assert "temporal.service" in env_res.stdout
    assert not pathlib.Path("/tmp/admin-env-bypassed").exists()


def start_https_git_repo() -> tuple[str, str, http.server.ThreadingHTTPServer]:
    git_root = pathlib.Path("/tmp/git-https")
    (git_root / "work").mkdir(mode=0o700, parents=True, exist_ok=True)
    srv_repo = git_root / "srv/demo-lab.git"
    srv_repo.mkdir(mode=0o700, parents=True, exist_ok=True)

    ca_crt = git_root / "ca.crt"
    srv_key = git_root / "server.key"
    subprocess.run(
        [
            "openssl",
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-keyout",
            str(srv_key),
            "-out",
            str(ca_crt),
            "-days",
            "2",
            "-subj",
            "/CN=git.lab.internal",
            "-addext",
            "subjectAltName=DNS:git.lab.internal",
        ],
        check=True,
        capture_output=True,
    )

    merged_ca = git_root / "merged-ca.crt"
    orig_ca = pathlib.Path("/etc/ssl/certs/ca-certificates.crt").read_bytes()
    merged_ca.write_bytes(orig_ca + b"\n" + ca_crt.read_bytes())
    subprocess.run(
        ["mount", "--bind", str(merged_ca), "/etc/ssl/certs/ca-certificates.crt"],
        check=True,
    )

    subprocess.run(["git", "init", "--bare", "--quiet", str(srv_repo)], check=True)
    v2_real = os.path.realpath("/run/booted-system/specialisation/v2")
    work = git_root / "work"
    (work / "flake.lock").write_text(
        json.dumps({"nodes": {"root": {}}, "root": "root", "version": 7}) + "\n"
    )

    builder_sh = "\n".join(
        [
            "set -eu",
            "export PATH=/run/current-system/sw/bin",
            'mkdir -p "$out/bin"',
            f'for item in "{v2_real}"/*; do',
            '  base=$(basename "$item")',
            '  if [ "$base" != "bin" ] && [ "$base" != "activate" ]; then',
            '    ln -s "$item" "$out/$base"',
            "  fi",
            "done",
            f'sed "s|{v2_real}|$out|g" "{v2_real}/activate" > "$out/activate"',
            'chmod +x "$out/activate"',
            f'ln -s "{v2_real}/bin/.switch-to-configuration-wrapped" "$out/bin/.switch-to-configuration-wrapped"',
            f'sed "s|{v2_real}|$out|g" "{v2_real}/bin/switch-to-configuration" > "$out/bin/switch-to-configuration"',
            'chmod +x "$out/bin/switch-to-configuration"',
        ]
    )
    flake_nix = (
        "{ outputs = { self }: { nixosConfigurations.lab-vm.config.system.build.toplevel = derivation { "
        'name = "nixos-system-lab-vm-v2"; system = "x86_64-linux"; builder = "/bin/sh"; '
        f'args = [ "-c" {json.dumps(builder_sh)} ]; '
        "}; }; }\n"
    )
    (work / "flake.nix").write_text(flake_nix)

    subprocess.run(["git", "-C", str(work), "init", "--quiet"], check=True)
    subprocess.run(["git", "-C", str(work), "config", "user.name", "Test"], check=True)
    subprocess.run(["git", "-C", str(work), "config", "user.email", "test@example.org"], check=True)
    subprocess.run(["git", "-C", str(work), "checkout", "-q", "-b", "downstreams/demo-lab"], check=True)
    subprocess.run(["git", "-C", str(work), "add", "flake.nix", "flake.lock"], check=True)
    subprocess.run(["git", "-C", str(work), "commit", "-q", "-m", "Reviewed v2"], check=True)
    rev_reviewed = subprocess.check_output(
        ["git", "-C", str(work), "rev-parse", "HEAD"], text=True
    ).strip()
    subprocess.run(
        ["git", "-C", str(work), "push", "--quiet", str(srv_repo), "downstreams/demo-lab"],
        check=True,
    )

    subprocess.run(["git", "-C", str(work), "checkout", "-q", "-b", "unreviewed"], check=True)
    with open(work / "flake.nix", "a") as f:
        f.write("# unreviewed\n")
    subprocess.run(["git", "-C", str(work), "add", "flake.nix"], check=True)
    subprocess.run(["git", "-C", str(work), "commit", "-q", "-m", "Unreviewed"], check=True)
    rev_unreviewed = subprocess.check_output(
        ["git", "-C", str(work), "rev-parse", "HEAD"], text=True
    ).strip()
    subprocess.run(
        ["git", "-C", str(work), "push", "--quiet", str(srv_repo), "unreviewed"],
        check=True,
    )
    subprocess.run(["git", "-C", str(srv_repo), "update-server-info"], check=True)

    handler = functools.partial(
        http.server.SimpleHTTPRequestHandler, directory=str(git_root / "srv")
    )
    httpd = http.server.ThreadingHTTPServer(("127.0.0.1", 8443), handler)
    tls_ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    tls_ctx.load_cert_chain(str(ca_crt), str(srv_key))
    httpd.socket = tls_ctx.wrap_socket(httpd.socket, server_side=True)
    thread = threading.Thread(target=httpd.serve_forever, daemon=True)
    thread.start()
    return rev_reviewed, rev_unreviewed, httpd


def test_safe_updates_drain_switch_and_recovery(temporal_cli: str) -> None:
    print("[vm-suite] 4/4 verifying HTTPS repo/branch bounds, drain barrier, switch, & recovery...")
    rev_reviewed, rev_unreviewed, httpd = start_https_git_repo()
    try:
        subprocess.run(
            [
                "ip",
                "netns",
                "exec",
                "boreal-internal",
                temporal_cli,
                "schedule",
                "create",
                "--address",
                "127.0.0.1:7233",
                "--namespace",
                "alice",
                "--schedule-id",
                "maint-sched-1",
                "--workflow-id",
                "sched-wf",
                "--type",
                "DemoWorkflow",
                "--task-queue",
                "alice-greet",
                "--interval",
                "24h",
                "--input",
                '{"name":"Sched","wait_seconds":1}',
            ],
            check=True,
            capture_output=True,
        )

        bad_repo = subprocess.run(
            [
                "boreal-updater",
                "schedule",
                "https://github.com/evil/fork.git",
                rev_reviewed,
                "lab-vm",
            ],
            capture_output=True,
            text=True,
        )
        assert bad_repo.returncode != 0

        subprocess.run(ssh_cmd("admin", f"update schedule {rev_unreviewed}"), check=True)
        unrev = subprocess.run(ssh_cmd("admin", "update prebuild"), capture_output=True, text=True)
        assert unrev.returncode != 0, "unreviewed commit unexpectedly passed prebuild"

        subprocess.run(ssh_cmd("admin", f"update schedule {rev_reviewed}"), check=True)
        subprocess.run(ssh_cmd("admin", "update prebuild"), check=True)
        pre_st = subprocess.check_output(ssh_cmd("admin", "update status"), text=True)
        assert "phase=prebuilt" in pre_st, pre_st
        assert not pathlib.Path("/etc/boreal-updated-generation").exists()

        alice = SshPtySession("alice")
        try:
            alice.wait_for(b"Boreal lab shell (alice)")
            # Parent workflow spawns a child workflow with a 15s timer + activity!
            alice.send(b"start greet drain-parent-1 'child:Grace' 15\r")
            alice.wait_for(b"started DemoWorkflow 'drain-parent-1'")

            apply_proc = subprocess.Popen(
                ssh_cmd("admin", "update apply"),
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
            )

            cur_st = ""
            for _ in range(50):
                cur_st = subprocess.check_output(["boreal-updater", "status"], text=True)
                if "phase=draining" in cur_st:
                    break
                time.sleep(0.2)
            else:
                raise AssertionError(f"updater did not enter draining phase: {cur_st}")

            # Concurrent start must be rejected while gate is closed!
            alice.send(b"start greet race-blocked-1 'Blocked' 1\r")
            alice.wait_for(b"admissions are temporarily closed")

            # Admin service mutation must be blocked while updater holds lock!
            svc_mut = subprocess.run(
                ssh_cmd("admin", "service restart temporal"), capture_output=True, text=True
            )
            assert svc_mut.returncode != 0

            # Schedule must be paused during drain!
            sched_during = json.loads(
                subprocess.check_output(
                    [
                        "ip",
                        "netns",
                        "exec",
                        "boreal-internal",
                        temporal_cli,
                        "schedule",
                        "describe",
                        "--address",
                        "127.0.0.1:7233",
                        "--namespace",
                        "alice",
                        "--schedule-id",
                        "maint-sched-1",
                        "--output",
                        "json",
                    ],
                    text=True,
                )
            )
            paused_during = sched_during.get("schedule", sched_during)["state"].get("paused", False)
            assert paused_during is True, sched_during

            out, err = apply_proc.communicate(timeout=90)
            assert apply_proc.returncode == 0, f"update apply failed: {out}\n{err}"
        finally:
            alice.close()

        # Re-bind merged CA if /etc was switched by switch-to-configuration
        assert (
            pathlib.Path("/etc/boreal-updated-generation").read_text().strip() == "v2-activated"
        )
        post_st = subprocess.check_output(ssh_cmd("admin", "update status"), text=True)
        assert "phase=activated" in post_st, post_st

        sched_after = json.loads(
            subprocess.check_output(
                [
                    "ip",
                    "netns",
                    "exec",
                    "boreal-internal",
                    temporal_cli,
                    "schedule",
                    "describe",
                    "--address",
                    "127.0.0.1:7233",
                    "--namespace",
                    "alice",
                    "--schedule-id",
                    "maint-sched-1",
                    "--output",
                    "json",
                ],
                text=True,
            )
        )
        paused_after = sched_after.get("schedule", sched_after)["state"].get("paused", False)
        assert paused_after is False, sched_after

        alice_post = SshPtySession("alice")
        try:
            alice_post.wait_for(b"Boreal lab shell (alice)")
            alice_post.send(b"result drain-parent-1\r")
            alice_post.wait_for(b"Hello, Grace!")

            alice_post.send(b"start greet post-switch-1 'Switched' 1\r")
            alice_post.wait_for(b"started DemoWorkflow 'post-switch-1'")
            for _ in range(30):
                time.sleep(0.4)
                alice_post.send(b"result post-switch-1\r")
                res = alice_post.wait_for(b"run ID:")
                if b"Hello, Switched!" in res:
                    break
            else:
                raise AssertionError("post-switch-1 did not finish after switch")
        finally:
            alice_post.close()

        # Verify interrupted-activation recovery fails closed when /run/current-system != result!
        state_file = pathlib.Path("/var/lib/boreal-updater/state")
        raw_state = state_file.read_text()
        unswitched = raw_state.replace("phase=activated", "phase=activating").replace(
            "result=/nix/store/",
            "result=/nix/store/00000000000000000000000000000000-",
        )
        state_file.write_text(unswitched)
        rec = subprocess.run(ssh_cmd("admin", "update recover"), capture_output=True, text=True)
        assert rec.returncode != 0
        assert "phase=blocked" in state_file.read_text()
    finally:
        httpd.shutdown()


def main() -> None:
    if len(sys.argv) != 2:
        raise SystemExit("usage: vm-suite.py <temporal-cli-path>")
    temporal_cli = sys.argv[1]
    setup_ssh_keys()
    test_host_and_worker_isolation(temporal_cli)
    test_ssh_shell_permissions_secrets_and_recovery()
    test_restricted_admin_and_bypass_resistance()
    test_safe_updates_drain_switch_and_recovery(temporal_cli)
    print("[vm-suite] all isolation, shell, admin, and update checks passed!")


if __name__ == "__main__":
    main()
