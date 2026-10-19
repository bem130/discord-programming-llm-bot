# Publication review (2026-10-19)

This review covers the source tree and history published to
`bem130/discord-programming-llm-bot`. Automated checks cannot guarantee the absence
of every vulnerability or credential. Live Discord/OpenRouter integration and VM
provisioning were not run as part of this review.

## Licensing

The package already declared MIT; `LICENSE` supplies the corresponding text.
Git history lists bem130 as the author. Dependencies retain their own licenses;
the project's MIT license does not replace them. `deny.toml` checks the resolved
dependency graph against an explicit license list and rejects unknown sources.
No dependency source or VM image is vendored in this repository. Distributing a
compiled binary or a provisioned VM requires preserving the applicable upstream
license notices separately. Tools downloaded during VM provisioning have their
own licenses and are outside the Cargo license check.

## Credentials and runtime data

Gitleaks 8.30.1 scans the Git history with fully redacted output. Its only initial
findings were the known dummy Discord guild ID in README and `.env.example`.
The exact dummy value is allowlisted; other IDs and credential rules remain
enabled. Git history contains no real `.env` file or runtime database/log file.
Runtime databases, SQLite sidecars, and logs are ignored by Git.

Only Git-managed content is published. Ignored local files are neither inspected
nor uploaded. The history scan does not inspect local runtime data or establish
whether credentials have been exposed through other channels.

## Dependency security

The initial lockfile audit reported 16 advisories. Serenity 0.12.5's Rustls
WebSocket feature selected Rustls 0.22, which has no patched release for the
reported advisories. The supported native TLS feature removes that dependency
and uses the platform TLS implementation and trust store. On Linux, builds need
OpenSSL development packages and pkg-config.

SQLite does not use TLS, so SQLx now enables only the Tokio runtime feature.
The current Rustls, webpki, Quinn, and yanked ChaCha20 lockfile entries were
updated. `cargo deny check advisories licenses sources` checks the resolved graph
without advisory exceptions.

SQLx's optional MySQL support can still leave `rsa` in Cargo.lock; it is not in
the resolved build graph for this SQLite application. A whole-lockfile
`cargo audit` can therefore report RUSTSEC-2023-0071 even though RSA is not
compiled. Enabling MySQL requires a new review; do not suppress that advisory
for an active RSA dependency.

## Application boundaries and ongoing checks

Source inspection covered administrative permission checks, guild/channel
authorization, Discord CDN attachment restrictions and byte limits, and Lima
process/resource restrictions. The remaining shared-context, provider-data,
network, and persistent-guest boundaries are documented in `SECURITY.md` and
README. This review is not a penetration test.

GitHub Actions checks formatting, tests, and Clippy on Windows and Linux. A
separate job checks dependency advisories, licenses, sources, and Git history on
push, pull requests, and a weekly schedule. Third-party Actions use pinned commit
IDs; the Gitleaks download uses a pinned version and SHA-256 digest. CI needs no
Discord token or OpenRouter API key. Private vulnerability reporting is enabled
on the publication repository.
