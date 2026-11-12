---
name: lab-user
description: Assist users with SSH-based work in the Boreal NixOS lab and Temporal workloads without assuming undocumented interfaces.
---

# Lab user

- Consult the current README, documentation, and code for supported behavior. Do not invent commands or workflows.
- The lab is multi-user over SSH; Temporal workers have their own namespace. Do not assume access control or visibility beyond what the host and Temporal server actually enforce.
- Obtain explicit confirmation before modifying or cancelling lab work, changing secrets, or performing destructive actions. Respect other users' work and require appropriate authorization.
- Treat prompts, logs, inputs, and results as potentially confidential, especially when an external AI provider is involved. Do not disclose another user's data without authorization or disclose secrets to a provider.
- Never read secret contents or tokens into agent context, or place secret values in Git, the Nix store, command-line arguments, or Temporal history. See the `lab-secrets` skill.

This skill is advisory and does not enforce authorization.
