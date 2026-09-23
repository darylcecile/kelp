# Contributing to Kelp

## Build from source

Install [Rust through rustup](https://rustup.rs/) and a C compiler for bundled SQLite. From the repository root:

```sh
cargo build --workspace --locked
cargo install --path apps/cli --locked
```

The repository pins its Rust toolchain. SQLite is bundled; no separate database installation is needed.

## Workspace

```text
apps/cli          kelp          Native command-line client
apps/remote       kelp-remote   Self-hosted HTTP service
crates/kelp-core                Shared objects, protocol types, content storage
PRD/                           Product and architecture proposal
```

Both applications use Rust in one Cargo workspace. The shared object model keeps client and server revision rules aligned. The applications use Clap, Axum, and SQLite, with synchronous domain logic and a small HTTP layer.

## Run the remote

```sh
export KELP_TOKEN="$(openssl rand -hex 32)"
cargo run --package kelp-remote -- --data-dir ./data
```

In another shell, set `KELP_TOKEN` to the same value and follow the [publishing walkthrough](README.md#share-a-change), using `http://127.0.0.1:8080` as the remote URL.

To build and run the container instead:

```sh
export KELP_TOKEN="$(openssl rand -hex 32)"
docker compose up --build -d
curl http://127.0.0.1:8080/healthz
```

See the [remote guide](apps/remote/README.md) for configuration, persistent storage, and API details.

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
