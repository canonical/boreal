# Run and inspect workflows

If you have a lab account, connect with `ssh alice@lab.example` (substitute
your own username and host). Boreal opens a restricted workflow shell, not a
general-purpose Unix shell. To try the same workflow interface without a
deployed lab, use the [local lab](local.md).

## Use the interactive shell

Type `workers` to see which workers and workflow aliases you can launch.
For example, the starter lab has an `example` worker with one public workflow:

```text
run example --id job-42
```

The form asks for input and shows a summary. Nothing starts until you confirm.
Press Enter to accept a default, Esc to edit a previous answer, or Ctrl+C to
abandon the form. In a choice list, Up/Down moves the selection and Left/Right
changes pages. Field help comes from the Rust input type's doc comments.

- If a worker exports several workflows, choose an alias in the form or pass
  `--workflow ALIAS`. The worker name is the only positional argument to `run`.
- If you omit `--id`, Boreal generates a workflow ID.
- Use `--input-json` to skip the form or to supply input the form cannot
  represent. Use `--no-ask` when you want missing required values to produce an
  error rather than a prompt.

After submission, use `list` to find executions, `status job-42` to check the
state, `view job-42` to inspect a live run, or `result job-42` to see the output
when it is available. These commands respect lab permissions. If you have
permission to cancel a workflow, `cancel job-42` asks for confirmation (`[y/N]`).
Use `permissions` to inspect the policy visible to your account.

## Run from a script

Use non-interactive SSH without a pseudo-terminal. It does not prompt:

```sh
printf '%s\n' '{"message":"hello"}' |
  ssh -T alice@lab.example 'run example --id job-42 --input-json -'
ssh -T alice@lab.example 'status job-42'
ssh -T alice@lab.example 'result job-42'
```

`--input-json -` reads from stdin, avoiding remote command-line quoting for
the payload. `run` returns after submission, not completion. Successful command
output goes to stdout; errors go to stderr with a nonzero exit status.

## History and secrets

In a deployed lab, `~/boreal.history` keeps up to 2,000 interactive command
selectors, not workflow inputs or scripted invocations. Up Arrow can recall
an exact `run` line (up to 16 KiB) during the same session. After reconnecting,
only the selectors remain.

Never put secrets in workflow inputs or results: Temporal retains that data.
Use `secrets set NAME` at the Boreal prompt to enter a value without echo, or
follow the [SSH secret provisioning example](host.md#user-secrets-and-restricted-administration).
