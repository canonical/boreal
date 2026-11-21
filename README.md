# Dispatch Desk

A small, offline Boreal lab for manual testing. Open a request at Alice's
desk, send it to the shared dispatch desk, and get back a receipt.

```text
Alice's desk ──→ Alice's vault checks desk_badge
             └─→ shared dispatch ──→ shared vault checks dispatch_key
                                └─→ non-secret receipt
```

Each vault checks only a disposable fixture. No token, token length, or
token-derived value is sent to Temporal. There are no external services.

| Runtime | What to test |
| --- | --- |
| QEMU VM (x86-64) | Two SSH users and Temporal namespaces, distinct vault stores, and one-way delegation: Alice may start/read shared dispatch. Shared cannot run Alice's desk. |
| Local Podman | The same desk, dispatch, and vault binaries, interactive forms, scripted input, and persistence. Both workers run in `local` with one fixture store; this is **not** a permissions test. |

## Use the working framework checkout

Run the examples from this worktree. `../..` is the enclosing Boreal checkout;
point it at your framework checkout if you cloned this branch elsewhere. The
override is needed until the pinned framework revision provides `boreal-sdk`.
It does not modify the lock file.

```sh
framework=$(realpath ../..)
local_lab() {
  nix run .#boreal-local --override-input boreal "$framework" --no-write-lock-file -- "$@"
}
```

## Local Podman

Use a regular Linux user with working rootless Podman. Set up the lab's private
relative mount once. These example strings are **test fixtures**, not real
credentials.

```sh
fixture_dir=$(local_lab secret-path)
(umask 077; mkdir -p -- "$fixture_dir")
printf '%s' 'demo-only-desk' | local_lab secrets set desk_badge
printf '%s' 'demo-only-dispatch' | local_lab secrets set dispatch_key
local_lab
```

In the shell, `run desk` opens the request form. Fill in a requester, pick a
zone, and confirm. `list`, `view <id>`, and `result <id>` show the execution and
receipt. To test input and output without the form:

```sh
printf '%s' '{"requester":"Avery","zone":"north"}' |
  local_lab run desk --id desk-local --input-json -
local_lab result desk-local
```

`run` submits immediately. If the result is still running, try again. Use a
new ID for another run. The desk result includes `dispatch_namespace`,
`dispatch_workflow_id`, and a receipt such as `Dispatch confirmed for Avery in
the north zone.` `local_lab stop` stops the lab without deleting its
SQLite state, fixture secrets, or selector-only shell history. Reopen the shell
and use Arrow Up to verify only the selector was persisted, not its input.

## Disposable QEMU VM

Paste a disposable **public** SSH key into `boreal.vm.devAuthorizedKeys` in
`flake.nix`. The VM accepts that key as `alice`, `shared`, and `admin`; never
use shared SSH keys on a real lab or put a private key in the checkout.
Ensure loopback port `2222` is free (or
change `boreal.vm.sshPort`). Use a fresh disk for the new topology rather than
reusing an old demo image.

```sh
disk_dir=$(mktemp -d "$HOME/dispatch-desk.XXXXXXXX")
printf 'VM state: %s\n' "$disk_dir"
NIX_DISK_IMAGE="$disk_dir/dispatch-desk.qcow2" \
  nix run .#boreal-vm --override-input boreal "$framework" --no-write-lock-file
```

In another terminal, set `disk_dir` to the printed path. Use `ssh -i` in the
helper if the disposable key is not already in your agent.

```sh
vm_ssh() { ssh -p 2222 -o "UserKnownHostsFile=$disk_dir/known_hosts" "$@"; }
printf '%s' 'demo-only-desk' | vm_ssh alice@127.0.0.1 'secrets set desk_badge'
printf '%s' 'demo-only-dispatch' | vm_ssh shared@127.0.0.1 'secrets set dispatch_key'
vm_ssh alice@127.0.0.1
```

Manual checks:

1. As Alice, `run desk` and then `result <id>`. A receipt confirms that Alice's
   vault check, the cross-namespace handoff, and the shared vault check worked.
2. As Alice, `list --owner shared` and `result <dispatch-id> --owner shared`
   should show the delegated run. `run dispatch --owner shared` also opens the
   shared workflow directly.
3. As `shared`, `run dispatch` works, but `run desk --owner alice` is denied.
   In each user's shell, `secrets list` shows only that user's fixture.
4. On a fresh VM, seed only `desk_badge` and run the desk workflow before
   seeding `dispatch_key`: dispatch should fail, not fall back to Alice's
   store. Seed the missing fixture and submit a new run to verify recovery.

To shut down the disposable guest after testing, run
`vm_ssh admin@127.0.0.1 poweroff`.

## Checks

```sh
nix build .#checks.x86_64-linux.lab --override-input boreal "$framework" --no-write-lock-file
nix develop --override-input boreal "$framework" --no-write-lock-file
cargo fmt --manifest-path temporal/Cargo.toml --all -- --check
cargo test --manifest-path temporal/Cargo.toml --workspace --exclude demo-vault --locked
cargo test --manifest-path temporal/Cargo.toml --package demo-vault --locked
```

Vault and ordinary worker packages are built separately so Cargo features
cannot give an ordinary worker the vault's `read-secrets` feature.
