# Releasing Accordo

Accordo is published as `accordo` on crates.io, `accordo` in `mattsverse/homebrew-tap`, and
platform archives in `mattsverse/accordo` GitHub Releases. The executable is always `accordo`.
Mise installs those same archives using `github:mattsverse/accordo`.
On npm, `@matfire/accordo` supplies the launcher and selects one of four optional
binary packages: `@getaccordo/accordo-darwin-arm64`, `@getaccordo/accordo-darwin-x64`,
`@getaccordo/accordo-linux-arm64-gnu`, and `@getaccordo/accordo-linux-x64-gnu`.

## One-time setup

1. Keep the application and release configuration on `main` in `mattsverse/accordo`.
2. Make `mattsverse/accordo` public before releasing. The workflow checks visibility:
   Homebrew and unauthenticated mise users need public release downloads.
3. Initialize the already-created, public `mattsverse/homebrew-tap` repository with
   a README on its default branch. An empty repository cannot be checked out by the
   formula publishing job. No formula needs to be written manually.
4. In the Accordo repository's Actions secrets, configure `HOMEBREW_TAP_TOKEN` with a
   fine-grained GitHub token limited to `mattsverse/homebrew-tap`, with repository
   contents read/write. Approve it in the organization if required. The workflow's
   ordinary GitHub token only writes releases in the Accordo repository.
5. Sign into crates.io and verify your email. From the clean, committed first release
   checkout, run `cargo publish --dry-run --locked`, then `cargo publish --locked`
   using your locally configured Cargo authentication. This first publication creates
   the crate; do it only when v0.1.0 is ready. Do not paste credentials into chat or
   commit them to the repository.
6. In the new crate's Trusted Publishing settings configure:
   - GitHub owner: `mattsverse`
   - Repository: `accordo`
   - Workflow: `release.yml` (the calling workflow, not `publish-crate.yml`)
   - Environment: `crates-io`
7. Create the `crates-io` GitHub Actions environment. Permit release tags to deploy;
   an optional reviewer can gate later crate uploads. Subsequent publication uses
   `rust-lang/crates-io-auth-action` to obtain a short-lived token.

The [Rust announcement](https://blog.rust-lang.org/2025/07/11/crates-io-development-update-2025-07/)
describes the initial publication requirement. See the
[authentication action](https://github.com/rust-lang/crates-io-auth-action) for the
calling-workflow identity used by Trusted Publishing.

## npm Trusted Publishing setup

You need publish access to the `@matfire` scope and the `@getaccordo` organization.
All five packages are public. Create a GitHub Actions environment named `npm` in
`mattsverse/accordo`, allowing release tags. The workflow uses GitHub-hosted runners,
Node 24, and `id-token: write`; it does not use an npm token secret.

For each of the five packages, configure npm's Trusted Publisher settings with:

- GitHub organization: `mattsverse`
- Repository: `accordo`
- Workflow filename: `release.yml`
- Environment: `npm`
- Allow direct `npm publish`.

New packages need an initial authenticated publication before their package settings
can be configured. The first tag builds and tests the real package tarballs before
the npm publishing job. Download the `npm-wrapper` artifact and the four
`npm-<Rust target>` artifacts from that run. With local npm authentication, publish
the exact downloaded `getaccordo-*.tgz` files first, then `matfire-accordo-<version>.tgz`,
using `npm publish <file.tgz> --access public`. Do not publish placeholders or publish
directly from the `npm/` source directory. It is a template with version `0.0.0`;
the release workflow sets the actual version from Cargo and pins all optional
dependencies to that exact version.

Then configure the five trusted publishers and re-run failed jobs. The workflow
skips versions only when their registry integrity matches the exact tarball. Future
versions publish through OIDC, including provenance, with the four binary packages
published before the main package. Keep these tarballs available when retrying a
partially completed release; don't rebuild them and overwrite a published version.

See [npm's Trusted Publishing documentation](https://docs.npmjs.com/trusted-publishers/).

## Cut a release

1. On `main`, set the intended version in `Cargo.toml` and refresh `Cargo.lock`
   (`cargo check`). Update the README or add release notes as appropriate. The version
   tag must be exactly `v` followed by the package version.
2. Run the development checks from the README and:

   ```sh
   cargo publish --dry-run --locked
   dist plan --tag v0.1.0
   node --test npm/launcher.test.cjs
   ```

   Substitute the intended version. For the first release, complete the crates.io
   bootstrap above before pushing the tag.
3. Commit the release changes, push `main`, then create and push just that version tag:

   ```sh
   git tag -a v0.1.0 -m 'Release v0.1.0'
   git push origin v0.1.0
   ```

4. Watch the Release workflow. It validates the version and main ancestry, runs tests
   on all four platforms, verifies the crate package, builds archives with cargo-dist,
   and runs the packaged executable. It assembles the formula and checksums before
   publishing the GitHub release, then publishes the crate, updates the tap, and
   publishes npm's binary packages before its main package. It verifies npm, mise,
   and Homebrew installation on each platform.

The initial crate upload is skipped when that version already exists and is not
yanked. A later tag uses Trusted Publishing to upload the new crate version.

## Platforms and tooling

The native build matrix comes from `dist-workspace.toml`: macOS ARM64 and x86_64,
and Linux ARM64 and x86_64. Linux builds use Ubuntu 24.04 and require glibc 2.39 or
newer. Archives contain the binary, README and license; the package includes source,
tests and the Cargo lockfile, excluding repository automation and local tooling.

Rust is pinned in `rust-toolchain.toml`, `mise.toml`, and the workflow setup steps.
Cargo-dist 0.30.3 is pinned in `dist-workspace.toml` and `mise.toml`. Update these
pins together. The workflow is intentionally maintained by hand, with explicit
publication dependencies; `allow-dirty = ["ci"]` prevents cargo-dist from requiring
its generated workflow. Do not run `dist init` or `dist generate` over release.yml.
Use `dist plan` and `dist build` to validate packaging, and actionlint to validate
workflow changes.

Release automation uses cargo-dist's JSON plan, `jq`, and the GitHub CLI directly.
Mise installs the pinned cargo-dist version from `mise.toml`; there are no custom
Python release helpers or installer actions. The Python terminal smoke test remains
part of application CI because it exercises a real pseudo-terminal and process cleanup.
Dependabot checks GitHub Actions versions weekly and groups their update PRs.

The npm packaging follows [Orhun's optional-dependency approach](https://blog.orhun.dev/packaging-rust-for-npm/).
It reuses cargo-dist's native binaries, with `os`/`cpu` filters and a glibc filter for
Linux. The small JavaScript launcher forwards arguments, terminal IO and signals;
there is no TypeScript build, runtime dependency, or install script. Platform manifests
come from `npm/package.tmpl`, a `jq` template that receives the name, version, OS and
architecture and reads shared metadata from `npm/package.json`. The main package's
version and optional dependency versions are also set with `jq`. Node 24 is used in CI;
the launcher supports Node 22 and newer.

Every release platform installs the generated tarballs offline with lifecycle scripts
disabled and runs the terminal smoke test through the npm launcher before publishing.
To run that smoke test against another installation, set `ACCORDO_BINARY` to its
absolute executable path when running `python3 tests/terminal_smoke.py`.

## Prereleases and failures

Tags such as `v0.2.0-rc.1` create GitHub prereleases. They do not publish to npm or crates.io,
update the stable tap, or replace GitHub's latest stable release.

Release runs are serialized to protect the tap from concurrent updates. Push versions
in increasing order; do not publish an older stable tag after a newer release.

If a build or validation step fails, no GitHub release is published. GitHub assets
are uploaded to a draft before it becomes public. Crates.io follows GitHub, then
Homebrew follows crates.io, so a later failure can leave earlier destinations live.
Npm publishing also follows GitHub, independently of crates.io and Homebrew; a failed
binary package upload prevents publication of the main npm package.
Fix missing credentials or account configuration and use GitHub Actions' **Re-run
failed jobs**. Already-published crate versions are skipped; unchanged formula commits
are skipped. A network or authorization error is never treated as a missing crate.

Do not re-run all jobs after a GitHub release is public: the workflow refuses to
replace its archives. For a code or packaging correction, commit the fix, bump the
version, and release a new tag. Never move a published tag. Installation smoke tests
run after publication and report failures; they cannot undo a published crate.
