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

The flake exposes `lab-worker`, `lab-worker-alt`, `lab-client`, and
`boreal-updater` packages. Each worker has its own Nix derivation. The
`nixosConfigurations.poc` host is an evaluation/build target for a container,
not a hardware configuration to install on a real machine.

## NixOS modules

Import `nixosModules.default` to add the BFSU and SJTU substituters while
retaining the default cache. The POC workers are disabled by default; enabling
them requires explicitly acknowledging that Temporal authorization is missing.
`boreal.poc.users` declares demo accounts and SSH public keys, and
`boreal.poc.workers` declares their packages and non-secret environments. The
module does not install or configure a Temporal server.

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
