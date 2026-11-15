# Agent guidance

- Read `README.md`, relevant documentation, and existing code when present; they are the source of truth for implemented behavior. Do not invent commands, interfaces, or workflows.
- `main` provides the Boreal NixOS/Temporal framework (`lib/`, `modules/`, `packages/`, `templates/`, `worker/`, `shell/`, `updater/`), while concrete lab deployments live in downstream flakes (either standalone repositories from `templates.default` or `downstreams/<name>` orphan branches checked out as worktrees under gitignored `downstreams/`). Follow the actual repository structure as it develops.
- The lab is multi-user over SSH, with Temporal workers in their own namespace. Do not assume namespace or user isolation is enforced unless the server and host configuration establish it. Administrators are trusted.
- User-managed secrets live under `~/.config/boreal/secrets`: directory mode `0700`, file mode `0600`. Never read secret contents or tokens into agent context. Never put secret values in Git, the Nix store, command-line arguments, or Temporal history.
- Treat user data, logs, and prompts as potentially confidential, including when using an external AI provider. Do not disclose other users' data without authorization; never disclose secrets to a provider.
- Confirm before modifying or cancelling lab work, changing secrets, or taking destructive actions. Do not perform an unsafe NixOS rebuild or automatic garbage collection.
- Run relevant existing tests and checks for changes when feasible; report what ran and any limitations. Avoid assuming a test command exists before checking the project.

Portable, advisory skills are in `.agents/skills/`: `lab-user`, `lab-secrets`, and `lab-admin`. They guide agents; they do not enforce authorization.
