# Boreal `downstreams/demo-lab` deployment

Reference downstream lab consuming `github:canonical/boreal`. This orphan
branch (`downstreams/demo-lab`) lives in gitignored `downstreams/demo-lab`
inside a Boreal checkout (or standalone with `--override-input boreal <url>`).

**This is a development example, not a secure shared lab.** Runtime permission
enforcement and Temporal isolation are not wired up yet, and `boreal-vm` shares
a test SSH public key across `alice`, `bob`, and `admin`.

## Development and checks

```sh
nix develop
cargo test --manifest-path temporal/Cargo.toml --workspace --locked
nix flake check
```

## Run the disposable VM

```sh
mkdir -p "$HOME/boreal-vm"
NIX_DISK_IMAGE="$HOME/boreal-vm/boreal-vm.qcow2" nix run .#boreal-vm
```

In another terminal on the host:

```sh
ssh -i ~/.ssh/self -p 2222 alice@127.0.0.1
cat /etc/boreal/workers.yaml
cat /etc/boreal/users/alice/perms.yaml
export TEMPORAL_ADDRESS=127.0.0.1:7233
export TEMPORAL_NAMESPACE=alice
LAB_TASK_QUEUE=alice-greet lab-client start greet alice-run-1 'Alice Smith' 5
LAB_TASK_QUEUE=alice-greet lab-client status alice-run-1
```
