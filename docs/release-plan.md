# Release process proposal

Researched 2026-09-05. On 2026-09-06, the user renamed the project to Accordo.
The current names are `mattsverse/accordo`, the `accordo` crate and command, and
`accordo` in `mattsverse/homebrew-tap`. Work stays on `main` using standard Git.
The crates.io API returned no existing `accordo` crate when checked on 2026-09-06.
Local release configuration is implemented; no publication has occurred.
See [the release procedure](releasing.md) for the current setup and operating guide.
The findings below record the original research.

## Current repository

- Duo is version 0.1.0, supports macOS and Linux, and has an MIT license.
- Existing CI runs formatting, linting, checks, tests, and a terminal smoke test on both operating systems.
- Cargo.toml lacks publishing metadata, including description, license declaration, and repository URL. `cargo package --list --allow-dirty` succeeds with a metadata warning; this is not a publish dry run.
- There are no commits or Git remotes at the time of inspection. Existing project files are untracked and should not be swept into a documentation-only checkpoint.
- Use standard Git, per the user's latest instruction.

## Recommended design

Use cargo-dist for version-tag-triggered GitHub release archives, checksums, and Homebrew formula updates. Add a separate crates.io publish job to the release workflow. Cargo-dist documents its [release pipeline](https://github.com/axodotdev/cargo-dist), [custom publish jobs](https://axodotdev.github.io/cargo-dist/book/reference/config.html), and [Homebrew integration](https://axodotdev.github.io/cargo-dist/book/installers/homebrew.html).

Proposed defaults: first stable release v0.1.0; macOS Apple Silicon and Intel, Linux ARM64 and x86_64. Validate each supported target before publishing. A release version must match Cargo.toml, and required checks and packaging validation must pass before any publication. Release retries should skip an already published crate version and permit retrying failed downstream distribution steps. Keep prereleases out of the stable tap.

Mise can install directly with `mise use -g github:OWNER/duo@latest`. Its GitHub backend selects release assets by platform; use predictable Rust target names and verify installation against the actual archives. A separate plugin is unnecessary. A short registry name can be proposed upstream later. See [mise GitHub backend](https://mise.jdx.dev/dev-tools/backends/github.html) and [registry](https://mise.jdx.dev/registry.html).

## Names and access needed

1. GitHub owner and repository, preferably a public `OWNER/duo`, with Actions enabled and access to configure the release workflow.
2. A crates.io package name: `duo` belongs to an unrelated existing project. The public API returned no crate for `duo-cli` at inspection time; availability is not a reservation. Keep the installed executable named `duo` using an explicit binary target if the package name changes. Sources: [duo API record](https://crates.io/api/v1/crates/duo), [duo-cli lookup](https://crates.io/api/v1/crates/duo-cli), [Cargo target names](https://doc.rust-lang.org/cargo/reference/cargo-targets.html#the-name-field).
3. A crates.io account with verified email and authorization for the initial publish. The documented bootstrap publishes the first version manually, then configures Trusted Publishing for the chosen GitHub workflow. Subsequent releases use short-lived authentication instead of a stored crates.io publish token. See [Rust Trusted Publishing announcement](https://blog.rust-lang.org/2025/07/11/crates-io-development-update-2025-07/) and [2026 update](https://blog.rust-lang.org/2026/01/21/crates-io-development-update/).
4. A Homebrew tap repository, suggested `OWNER/homebrew-tap`, plus repository-scoped write access from the release workflow. Cargo-dist's documented setup uses a GitHub Actions secret named `HOMEBREW_TAP_TOKEN`; enter credentials through account/repository settings. See [Homebrew setup](https://axodotdev.github.io/cargo-dist/book/installers/homebrew.html).

## Implementation once names are chosen

Complete package metadata and explicit target names; define a package file allowlist; generate and pin cargo-dist configuration; add release checks and crates.io publishing; configure tap updates; document installation and the release procedure. Validate a publish dry run and local artifacts, then use GitHub CI for the full platform matrix and test the first released artifacts through Homebrew and mise.
