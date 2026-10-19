# Security policy

## Supported versions

Security fixes are made on the latest `main` branch. Older commits and local
deployments must be updated by their operators.

## Reporting a vulnerability

Use GitHub's private vulnerability reporting on this repository:
https://github.com/bem130/discord-programming-llm-bot/security/advisories/new

Do not post exploitable details, credentials, Discord messages, database files,
or logs containing user data in public issues. Include a minimal reproduction
using dummy credentials and synthetic messages. If private reporting is
unavailable, open an issue requesting a private reporting channel without
including vulnerability details.

## Deployment boundaries

- Keep Discord and OpenRouter credentials outside Git. Never share `.env` files.
  Revoke and replace any exposed credentials; deleting a file does not remove it
  from Git history.
- Channel access modes define a shared AI context within one configured guild.
  Content in enabled channels may appear in answers in other enabled channels.
  Set confidential channels to `off` and tell members how their data is used.
- SQLite stores messages and attachments without encryption. Restrict access to
  the database, backups, and runtime logs. Do not publish runtime directories.
- Messages and attachments may be sent to OpenRouter and its model providers.
  Operators are responsible for provider settings and applicable data policies.
- Code execution is disabled by default. The Lima runner is not a complete
  sandbox for hostile tenants: it has network access and persistent guest state.
  Use a dedicated host, network restrictions, quotas, and guest reset procedures
  before exposing execution to untrusted users. Keep credentials out of guests.
- VM provisioning downloads third-party software, including upstream install
  scripts. Review the template and upstream sources before provisioning.

See the README for the implemented limits and operational requirements.
