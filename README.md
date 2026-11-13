# Boreal lab POC

A NixOS and Temporal Rust proof of concept for an SSH-managed lab. The root
flake tracks `nixos-26.05`; `flake.lock` pins the selected revision. Update the
lock file deliberately rather than resolving a moving branch during deployment.

**This is not a secure shared-lab deployment.** The Temporal demo does not
configure authentication or server-side authorization. A development server on
localhost is still reachable by other local users. Namespaces, Unix accounts,
and agent skills do not by themselves enforce Temporal isolation.

The update service can schedule and prebuild a pinned revision, but it **never
switches the system**: it fails closed until an independently enforced
admission-and-drain protocol exists. It does not run garbage collection. See
[architecture](docs/architecture.md) and [updater](updater/README.md).

## Development

```sh
nix develop
cargo test --manifest-path temporal/Cargo.toml --workspace --locked
cargo test --manifest-path updater/Cargo.toml --locked
nix flake check
```

The shell includes Rust, `protoc`, the Temporal CLI, and Nix formatting tools.
See [Temporal POC](temporal/README.md) for running the disposable development
server, workers, and client. Use only non-secret test data. The server and demo
clients do not implement maintenance authorization.

## Local shell prototype

Run the mock interactive shell without a VM or Temporal server:

```sh
nix run .#boreal-shell
# Alternatively: cargo run --manifest-path shell/Cargo.toml --locked
```

Type `help` to see the mock commands. For example, try
`start greet run-1 'Alice Smith' 5 --owner bob`. Tab completes commands and
arguments; Up/Down navigate the current session's history. Previously entered
commands may appear as ghost suggestions; press Right to accept them. `exit` or
Ctrl-D closes the shell, and Ctrl-C clears the current input. History is kept
in memory only.

The users and workers are sample values. Commands only display their parsed
arguments: they never connect to Temporal, run programs, or modify lab work.
This is not a security boundary and must not be installed as a login shell.

For the shell's unit and terminal interaction checks:

```sh
cargo test --manifest-path shell/Cargo.toml --locked
cargo build --manifest-path shell/Cargo.toml --locked
python3 tests/shell-pty.py shell/target/debug/boreal-shell
```

The flake exposes `lab-worker`, `lab-worker-alt`, `lab-client`, `boreal-shell`,
and `boreal-updater` packages. Each worker has its own Nix derivation. The
`nixosConfigurations.poc` host is an evaluation/build target for a container,
not a hardware configuration to install on a real machine.

For a disposable QEMU guest with demo users and workers, see the
[VM instructions](docs/vm.md). The VM does not modify the host's NixOS
configuration and is not secure for a shared lab.

## NixOS modules

Import `nixosModules.default` to add the BFSU and SJTU substituters while
retaining the default cache. The POC workers are disabled by default; enabling
them requires explicitly acknowledging that Temporal authorization is missing.
`boreal.poc.users` declares demo accounts and SSH public keys, and
`boreal.poc.workers` declares their packages and non-secret environments. The
module does not install or configure a Temporal server.
See [the disposable host example](docs/poc-host.md) for a complete module
configuration and matching client queues.

Set `boreal.poc.updater.enable = true` to install the updater and its oneshot
service. A trusted root administrator can use `boreal-schedule-update` to
schedule a Git revision and start the service; see the updater documentation
for arguments. A successful prebuild deliberately reports `blocked` and the
service exits unsuccessfully: it has not activated anything. Failed or
blocked builds require administrative review, not automatic cleanup.

Users should provision their own secrets outside the repository and Nix store
under `~/.config/boreal/secrets`, with mode `0700` on the directory and `0600`
on the files. Do not put secret contents into Temporal inputs, results, errors,
or logs. Agent instructions are in `AGENTS.md` and `.agents/skills/`.
