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
apps/remote-cloudflare          Cloudflare Worker remote (TypeScript)
crates/kelp-core                Shared objects, protocol types, content storage
PRD/                           Product and architecture proposal
```

The native CLI and remote use Rust in one Cargo workspace. The shared object model keeps their revision rules aligned. The Cloudflare remote is a separate npm workspace using the same wire protocol; its integration tests exercise the actual Rust CLI against Wrangler's local runtime.

## Run the remote

```sh
export KELP_TOKEN="$(openssl rand -hex 32)"
cargo run --package kelp-remote -- --data-dir ./data
```

In another shell, set `KELP_TOKEN` to the same value and follow the [remote walkthrough](README.md#configure-a-remote-once), using `http://127.0.0.1:8080/my-project` as the project URL.

To build and run the container instead:

```sh
export KELP_TOKEN="$(openssl rand -hex 32)"
docker compose up --build -d
curl http://127.0.0.1:8080/healthz
```

See the [remote guide](apps/remote/README.md) for configuration, persistent storage, and API details.

## Cloudflare remote

Install Node.js 24+ and run from the repository root:

```sh
npm ci
cp apps/remote-cloudflare/.dev.vars.example apps/remote-cloudflare/.dev.vars
npm run dev --workspace @kelp/remote-cloudflare
```

Use `KELP_TOKEN=local-development-token` and `http://localhost:8787/my-project` with the CLI. Local state is kept in the Worker's ignored `.wrangler` directory. Change the local token in `.dev.vars` when needed; deployed tokens use a Wrangler secret.

```sh
npm run check --workspace @kelp/remote-cloudflare
npm run build --workspace @kelp/remote-cloudflare
npm test --workspace @kelp/remote-cloudflare
```

`build` is an offline deployment dry run. Tests build the Rust CLI and start isolated local R2/SQLite Durable Object storage. They check protocol rejection, atomic publication, real CLI large-file/sparse/conflict/tag workflows, and persistence across a Worker restart. Deployment instructions are in [the Cloudflare remote guide](apps/remote-cloudflare/README.md).

## Run the distributed proof

```sh
export KELP_TOKEN="$(openssl rand -hex 32)"
export KELP_STORAGE_TOKEN="$(openssl rand -hex 32)"
docker compose -f compose.cluster.yaml up --build -d
```

This runs one gateway and three storage containers with separate volumes. Only the gateway port is published. Point the ordinary CLI at `http://127.0.0.1:8080/my-project`; it does not need shard addresses.

The integration proof starts three independent HTTP stores, writes one project through two gateways, verifies physical object/journal placement, then adds a fourth store:

```sh
cargo test -p kelp-cli --test distributed --locked -- --nocapture
```

The test reports placement counts. It is a correctness/partitioning proof, not a production throughput benchmark.

## Run the checks

The root Cargo workspace is the source of truth for dependencies and toolchain settings.

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

To format changes, run `cargo fmt --all`. To exercise the client without installing it, use `cargo run --package kelp-cli -- --help`. The [README](README.md) has the local and client/server walkthroughs.

## Measure performance

```sh
cargo build --release --package kelp-cli --package kelp-remote --locked
python3 benchmarks/compare.py --output benchmarks/results/local.json
python3 benchmarks/network.py --output benchmarks/results/network.json
```

Both scripts use temporary projects and leave real workspaces alone. `--temp-dir` chooses the scratch directory. The [benchmark guide](benchmarks/README.md) defines the baseline settings, scope, and interpretation.

## Where changes belong

- `apps/cli`: workspace files, saved versions, command UX, and the remote client.
- `apps/remote`: transaction gateways, private storage nodes, and the standalone v0 compatibility API.
- `crates/kelp-core`: atomic edit transactions, dependency-closed views, conflict projection, and immutable content storage.

Keep protocol types shared and command-specific behavior in its application. Prefer small concrete functions to framework-style abstractions. Errors should explain what failed and whether local work remains available.

Add tests for behavior that could lose work or break interoperability. Coverage includes independent concurrent pushes, file-parent dependencies, multi-file atomicity, conflict resolutions, committed-only transfer, retries, and real multi-node placement.

## Format changes

The transaction protocol is `kelp/1`; the existing typed object-hash envelope remains stable so blob IDs can be reused. Changes to either require explicit compatibility decisions. Local schema upgrade preserves saved snapshots and archives old metadata; an explicit new commit introduces the desired files into transaction synchronization. Describe implemented behavior in the root README and future requirements in `PRD/`.

Workspace metadata version 3 fences the new file/path representation from older CLIs. Full saved views retain their original encoding; scoped saved views include selected paths in their hash. Sparse journals select relevant transactions, and clients retain every fetched transaction's complete dependency closure. Tests in `apps/cli/tests/partial.rs` cover expansion, merge, import, symlinks, chunked content, and recovery boundaries.

Extended file entries and imported provenance use transaction format 2. Large files use bounded trees of blob-backed chunk manifests; regular small files retain their existing hashes and encoding. `crates/kelp-core/src/paths.rs` encodes non-UTF-8 components without changing existing UTF-8 path identities. Release pins bind a name, snapshot, and causal roots and must match committed content.

The file-stat and projection caches are rebuildable derived data, not saved user versions. Unix file reuse checks device/inode, size, mode, mtime, and ctime and rereads racy timestamps; other platforms conservatively reread content. Object indexes use binary hashes and interned namespace/kind IDs; payloads live separately in loose rows or bounded packs. The storage migration validates and preserves older raw/compressed objects. Do not use an older binary to open an upgraded database.

`kelp gc` losslessly repacks objects and vacuums free pages. The remote's `--compact --data-dir PATH` option performs the same operation on a stopped storage node or standalone server. Packing never crosses a node or namespace. Deploy matching CLI/gateway/storage builds when using the batch endpoints.

## Release the CLI

Update the workspace version in `Cargo.toml` and the Kelp package versions in `Cargo.lock`, commit, then push a matching `v<version>` tag. `.github/workflows/release.yaml` builds the CLI for Linux, macOS, and Windows, verifies its reported version against the tag, and publishes the archives and checksums as a GitHub release.
