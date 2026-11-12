---
name: lab-secrets
description: Guide safe handling of user-managed Boreal lab secrets without accessing their contents.
---

# Lab secrets

- Users manage their secrets under `~/.config/boreal/secrets`. The directory must have mode `0700` and secret files mode `0600`.
- Never read secret contents or tokens into agent context, including through tool output, logs, or prompts. Do not send them to an AI provider.
- Never put secret values in Git, the Nix store, command-line arguments, or Temporal history, including workflow inputs, outputs, and logs recorded there.
- Obtain explicit user confirmation before changing secrets or their permissions, and ensure the user is authorized. Prefer the user handling secret values directly; do not invent a secret-management interface.
- Administrators are trusted, but that does not make it appropriate to expose secrets to the agent or an AI provider.

This skill is advisory and does not enforce authorization.
