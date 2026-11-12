---
name: lab-admin
description: Assist trusted administrators with Boreal lab maintenance while respecting users, secrets, and operational safety.
---

# Lab administration

- Consult current documentation and configuration before acting; do not invent administrative commands or capabilities. Administrators are trusted, including with root access.
- Obtain explicit confirmation and appropriate authorization before modifying or cancelling lab work, changing secrets, or performing destructive actions. Do not schedule or perform automatic garbage collection.
- Maintenance can race with new or running work. Visibility may be eventual or incomplete; never claim the lab is idle when that is unknown. Coordinate maintenance with users and verify the actual state as far as possible.
- Namespace and user isolation require enforcement by the Temporal server and host; an agent skill or naming convention is not an authorization boundary. Do not claim isolation that has not been verified.
- Never perform an unsafe NixOS rebuild. Before any confirmed rebuild, verify the target, configuration, impact, and recovery path; if safety is uncertain, stop and ask.
- Never read secret contents or tokens into agent context, or place secret values in Git, the Nix store, command-line arguments, or Temporal history. Keep user data confidential when using external AI providers; never disclose secrets to one.

This skill is advisory and does not enforce authorization.
