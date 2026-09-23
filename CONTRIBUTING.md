# Contributing to Kelp

## Run the checks

The root Cargo workspace is the source of truth for dependencies and toolchain settings.

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

To format changes, run `cargo fmt --all`. To exercise the client without installing it, use `cargo run --package kelp-cli -- --help`. The [README](README.md) has the local and client/server walkthroughs.

## Where changes belong

- `apps/cli`: workspace files, checkpoints, command UX, and the remote client.
- `apps/remote`: HTTP handling, authorization, and remote metadata transactions.
- `crates/kelp-core`: shared object definitions, validation, and immutable content storage.

Keep protocol types shared and command-specific behavior in its application. Prefer small concrete functions to framework-style abstractions. Errors should explain what failed and whether local work remains available.

Add tests for behavior that could lose work or break interoperability. Existing tests cover local-only recovery, first publication, revision continuity, network failure/retry, automatic checkpoints, content integrity, divergent heads, and remote authorization.

## Format changes

`kelp/0` is experimental and distinct from the PRD's proposed v1. Changes to hashing or persisted objects require explicit compatibility decisions. Describe implemented behavior in the root README; keep future requirements in `PRD/`.

## Release the CLI

Update the workspace version in `Cargo.toml` and the Kelp package versions in `Cargo.lock`, commit, then push a matching `v<version>` tag. `.github/workflows/release.yaml` builds the CLI for Linux, macOS, and Windows, verifies its reported version against the tag, and publishes the archives and checksums as a GitHub release.
