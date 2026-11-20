# Boreal downstream lab starter

Starter flake consuming `github:canonical/boreal`. `boreal.lib.buildRustWorker`
and `boreal.lib.mkDevShell` automatically link `${boreal}/worker` at
`.boreal/boreal-worker`, so `flake.lock` is the single source of truth for the
Boreal framework revision.

## Quick start

1. Create a new lab project (standalone repository):

   ```sh
   nix flake new my-lab -t github:canonical/boreal
   cd my-lab
   git init && git add .
   ```

   *(Or inside a Boreal checkout as an orphan worktree:
   `git worktree add --orphan -b downstreams/my-lab downstreams/my-lab`, then
   `cd downstreams/my-lab && nix flake init -t ../.. && git add .`.)*

2. Customize workflows and users:
   - Replace or add Temporal worker crates under `temporal/`.
   - Point `boreal.users.<name>.workers.<name>.package` at a worker output.
     `buildRustWorker` runs the `schema` bin and installs `schema.yaml`.
   - Declare lab users, public SSH keys, worker instances, and permissions under
     `boreal.users` in `lab.nix`.

3. Run checks and tests:

   ```sh
   nix flake check
   nix develop
   cargo test --manifest-path temporal/Cargo.toml --workspace --locked
   ```

   *(When testing against local unpushed commits on Boreal's `main` branch, pass
   `--override-input boreal ../.. --no-write-lock-file` to `nix` commands so
   local `file://` paths are not written into `flake.lock`.)*

4. Run real workflows locally (Linux with rootless Podman configured):

   ```sh
   nix develop -c boreal-local
   # In the prompt: run example
   ```

   Or submit directly with no prompts:

   ```sh
   nix develop -c boreal-local run example --id local-1 --input-json '{"message":"hello"}'
   nix develop -c boreal-local result local-1
   nix develop -c boreal-local stop
   ```

   Temporal, the broker, and the worker run locally on a private container
   network with no external access. See the [local lab guide](https://github.com/canonical/boreal/blob/main/docs/local.md)
   for profiles, vault testing, and prerequisites.

5. Test deployment behavior in the disposable QEMU VM:
   - Set `boreal.vm.devAuthorizedKeys` in `flake.nix` (or `users.<user>.keys` in
     `lab.nix`) to your public SSH key, then launch:

     ```sh
     nix run .#vm
     ```

   - SSH into the guest on `127.0.0.1:2222`:

     ```sh
     ssh -p 2222 alice@127.0.0.1
     ssh -p 2222 admin@127.0.0.1 services
     ssh -p 2222 admin@127.0.0.1 poweroff
     ```

6. Deploy and stage updates:
   - Add `./hosts/<hostname>.nix` (importing `hardware-configuration.nix`,
     setting `networking.hostName = "<hostname>";`, and configuring
     `boreal.admin` and `boreal.updater.enable = true;`) and wire
     `nixosConfigurations.<hostname>` in `flake.nix` using `boreal.lib.mkHost`.
   - Clone this repository at `/etc/nixos` on the host tracking your reviewed
     `https://` branch. Commit a remote-resolved `flake.lock` (via
     `nix flake lock` or `nix flake update boreal`), push to the remote branch,
     and run `ssh admin@<host> "updater update"` followed by
     `ssh admin@<host> "updater schedule"`.

**Security note:** Provision user secrets under `~/.config/boreal/secrets`
(`0700` directory, `0600` files) via `boreal-shell` (`secrets set <name>`).
Never place secrets in Git, the Nix store, command-line arguments, or Temporal
history.
