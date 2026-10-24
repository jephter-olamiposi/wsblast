# Releasing wsblast

Releases are built by [dist](https://opensource.axo.dev/cargo-dist/). Its
configuration lives in `dist-workspace.toml`, and `.github/workflows/release.yml`
is generated from it. Never edit `release.yml` by hand: change
`dist-workspace.toml`, run `dist generate` and commit both files. The release
workflow also runs `dist plan` on every pull request, which fails when the two
are out of sync.

## What a release tag produces

Pushing a tag `vX.Y.Z` whose version matches `version` in `Cargo.toml` runs
`release.yml`, which:

1. builds `wsblast` with the `dist` profile (defined in `.cargo/config.toml`,
   identical to `[profile.release]`) for:
   - `x86_64-unknown-linux-gnu`
   - `aarch64-unknown-linux-gnu` (native `ubuntu-24.04-arm` runner)
   - `x86_64-apple-darwin`
   - `aarch64-apple-darwin`
   - `x86_64-pc-windows-msvc`
2. creates a GitHub Release with `wsblast-<target>.tar.gz` (Unix) or
   `wsblast-<target>.zip` (Windows), a `.sha256` file for each archive,
   `sha256.sum`, a source tarball, and the `wsblast-installer.sh` and
   `wsblast-installer.ps1` installers. The release notes come from the
   matching section of `CHANGELOG.md`.
3. publishes the crate to crates.io through `publish-crates.yml`, using
   trusted publishing.

A tag with a prerelease suffix, such as `v0.2.0-rc.1`, creates a GitHub
**prerelease** and skips the crates.io job (`publish-prereleases = false`).
Prereleases are not returned by `releases/latest`, so `install.sh` and the
`latest/download` installer URLs keep pointing at the last stable release.

The archive names are what `install.sh` downloads and what
`[package.metadata.binstall]` in `Cargo.toml` describes, so
`cargo binstall wsblast` installs the prebuilt binary.

## One-time setup

### crates.io trusted publishing

`publish-crates.yml` exchanges the job's GitHub OIDC token for a short-lived
crates.io token with `rust-lang/crates-io-auth-action`, so no crates.io API
token is stored in GitHub.

1. Sign in to crates.io as an owner of `wsblast` and open
   <https://crates.io/crates/wsblast/settings>.
2. Under **Trusted Publishing**, add a GitHub publisher:
   - Repository owner: `jephter-olamiposi`
   - Repository name: `wsblast`
   - Workflow filename: `release.yml`
   - Environment: `crates-io`

   The workflow filename is `release.yml`, not `publish-crates.yml`:
   crates.io matches the top-level workflow from the token's `workflow_ref`
   claim, and `publish-crates.yml` runs as a reusable workflow called by
   `release.yml`.
3. Optionally enable "require trusted publishing for all new versions" in
   the same settings page once the first trusted publish has succeeded, and
   revoke any old API tokens used for manual publishing.

### GitHub

1. Create an environment named `crates-io` under **Settings > Environments**.
   Restrict its deployment branches and tags to `v*` tags, and add required
   reviewers if every crates.io publish should wait for approval.
2. Under **Settings > Actions > General**, keep "Read and write permissions"
   available to workflows (the release job needs `contents: write` to create
   the GitHub Release) and allow GitHub Actions to create releases.
3. If `main` has branch protection, add the CI jobs you want to require
   (for example `Rustfmt`, `Clippy (full)`, `Test (ubuntu-latest, full)`,
   `MSRV (default)`, `Docs`, `Package`, `End-to-end SLO gate`, `cargo-deny`).

### Optional: Homebrew tap

Homebrew publishing is off. To turn it on, follow the steps in the comment
above `tap` in `dist-workspace.toml`: create
`jephter-olamiposi/homebrew-tap`, add a `HOMEBREW_TAP_TOKEN` secret that can
push to it, enable the `homebrew` installer and publish job, then run
`dist generate`.

## Cutting a release

1. Make sure CI is green on `main`.
2. Set `version` in `Cargo.toml` and add a matching `## [X.Y.Z]` section to
   `CHANGELOG.md`. Run `cargo check` so `Cargo.lock` picks up the version.
3. Check the plan locally (optional): `dist plan --tag vX.Y.Z`.
4. Commit, then tag and push:

   ```bash
   git tag -a vX.Y.Z -m "wsblast X.Y.Z"
   git push origin vX.Y.Z
   ```

### Dry run with a release candidate

For v0.2.0, ship `v0.2.0-rc.1` first:

1. Set `version = "0.2.0-rc.1"` in `Cargo.toml`, add a `## [0.2.0-rc.1]`
   changelog section, commit, tag `v0.2.0-rc.1` and push the tag.
2. Confirm the GitHub prerelease has all five archives, their `.sha256`
   files and both installers, and that the crates.io job was skipped.
3. Try the artifacts:

   ```bash
   # dist shell installer for this exact prerelease
   curl -fsSL https://github.com/jephter-olamiposi/wsblast/releases/download/v0.2.0-rc.1/wsblast-installer.sh | sh
   # install.sh pinned to the prerelease
   curl -fsSL https://raw.githubusercontent.com/jephter-olamiposi/wsblast/main/install.sh | WSBLAST_VERSION=v0.2.0-rc.1 sh
   # cargo-binstall, reading the metadata from a local checkout
   cargo binstall --manifest-path . wsblast
   ```

   On Windows, run the PowerShell installer:
   `powershell -c "irm https://github.com/jephter-olamiposi/wsblast/releases/download/v0.2.0-rc.1/wsblast-installer.ps1 | iex"`.
4. Set `version = "0.2.0"`, rename the changelog section, commit, tag
   `v0.2.0` and push. This run publishes to crates.io.
