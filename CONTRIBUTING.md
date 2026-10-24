# Contributing to wsblast

Thanks for helping improve wsblast. Bug reports, feature ideas and pull
requests are all welcome. By taking part you agree to follow the
[Code of Conduct](CODE_OF_CONDUCT.md).

Security problems should not be filed as public issues; see
[SECURITY.md](SECURITY.md).

## Development setup

1. Install Rust with [rustup](https://rustup.rs). wsblast uses the 2024
   edition; development happens on the latest stable toolchain, and CI also
   checks the minimum supported version declared as `rust-version` in
   `Cargo.toml`.

   ```bash
   rustup toolchain install stable --component rustfmt --component clippy
   ```

2. On Linux, the default `native-tls` backend links OpenSSL, so you also need
   `pkg-config` and the OpenSSL headers (`libssl-dev` on Debian and Ubuntu).

3. Clone and build:

   ```bash
   git clone https://github.com/jephter-olamiposi/wsblast.git
   cd wsblast
   cargo build
   ```

### Trying it locally

`examples/echo_server.rs` is a local WebSocket echo server for manual testing:

```bash
cargo run --example echo_server            # listens on 127.0.0.1:9001
cargo run -- ws://127.0.0.1:9001 -c 10 -d 5s --max-p99 100ms
```

Set `ADDR` to change the listen address and `DELAY_MS` to add an artificial
response delay.

## Quality gate

CI runs the following on every pull request. Please run them before you push:

```bash
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo clippy --locked --all-targets --features full -- -D warnings
cargo test --locked --all-targets --features full
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps --all-features
```

CI additionally:

- runs clippy with each Cargo feature enabled on its own (see the `clippy`
  job matrix in `.github/workflows/ci.yml`), so check the feature you touched,
  for example `cargo clippy --all-targets --features scenario -- -D warnings`;
- runs the test suite on Linux, macOS and Windows;
- checks the declared `rust-version`;
- runs `cargo package` and `cargo publish --dry-run`;
- runs an end-to-end SLO gate against the echo server (exit code `0` for a
  passing run, `2` for a breached budget);
- runs `cargo deny check` (licenses, advisories, sources) and
  `cargo machete` (unused dependencies);
- builds the Docker image.

If you add a Cargo feature, add it to the `clippy` matrix in
`.github/workflows/ci.yml` (and to `full` in `Cargo.toml` when it should ship
in the all-features build). If you add a dependency, make sure its license is
allowed by `deny.toml`.

## Commit messages

Commits follow [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/):

```
<type>(<optional scope>): <summary in the imperative mood>

<body explaining what changed and why>
```

Common types are `feat`, `fix`, `perf`, `refactor`, `docs`, `test`, `build`,
`ci` and `chore`. Mark breaking changes with `!` after the type
(`feat!: ...`) or a `BREAKING CHANGE:` footer. Keep the summary under about 72
characters and use the body for the reasoning, measurements or trade-offs a
reviewer needs.

## Pull requests

- Keep each pull request focused on one change, and add tests for new
  behaviour and bug fixes.
- Update `README.md` when user-facing flags or output change, and add an entry
  under `## [Unreleased]` in `CHANGELOG.md`.
- Performance-sensitive changes to the worker loop or metrics should include
  before and after numbers (`cargo bench` runs the Criterion benchmarks in
  `benches/`).

## Releases

Maintainers cut releases as described in [RELEASING.md](RELEASING.md).

## License

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in wsblast by you, as defined in the Apache-2.0
license, shall be dual licensed under the MIT and Apache-2.0 licenses, without
any additional terms or conditions.
