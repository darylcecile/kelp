# Kelp

Local-first, change-oriented version control. Edit files, then publish; further publications update the same change.

This monorepo contains a working **v0 prototype**. The [PRD](PRD/README.md) describes the broader product direction. Current commands and limits below describe what is implemented today.

## Workspace

```text
apps/cli          kelp          Native command-line client
apps/remote       kelp-remote   Self-hosted HTTP service
crates/kelp-core                Shared objects, protocol types, content storage
PRD/                           Product and architecture proposal
```

Both applications use Rust in one Cargo workspace. Rust offers native performance and memory safety; sharing the object model prevents the client and server from implementing different revision rules. The code uses conventional Clap, Axum, and SQLite components with synchronous domain logic and a small HTTP layer.

## Build

Prebuilt CLI binaries are available from [GitHub Releases](https://github.com/darylcecile/kelp/releases) for Linux x86-64, macOS Intel/Apple Silicon, and Windows x86-64. Extract the archive and put `kelp` (or `kelp.exe`) on your `PATH`. Each release includes `SHA256SUMS` for verifying downloads.

Install [Rust through rustup](https://rustup.rs/), then:

```sh
cargo build --workspace --locked
cargo install --path apps/cli --locked
```

The repository pins its Rust toolchain. SQLite is bundled; no separate database installation is needed. A C compiler is required for the bundled SQLite build.

## Start entirely locally

```sh
mkdir my-project
cd my-project
kelp init

# Create files in your editor, then include them in version control:
kelp track src
kelp status
kelp log

# Optional named checkpoint:
kelp checkpoint -m "Before refactoring"

# Recover a checkpoint into a new directory:
kelp restore 1 --to ../recovered-project
```

`init` needs no network, token, or remote. It tracks existing regular files, respecting `.gitignore` and `.kelpignore`. Add later files with `kelp track PATH`. Tracked deletions remain part of subsequent snapshots.

Automatic checkpoints start with `init` and `open`. They capture tracked bytes saved by your editor, not unsaved editor buffers. `kelp status` reports the watcher, and `.kelp/watch.log` contains capture errors. Use `kelp watch --stop` to stop the process, `kelp watch` to restart it, or `--no-watch` on `init`/`open` in automation. All checkpoints are retained in v0.

## Run a remote and publish

From this repository, start the service:

```sh
export KELP_TOKEN="$(openssl rand -hex 32)"
cargo run --package kelp-remote -- --data-dir ./data
```

In your project, set the same token in the client shell and attach the remote:

```sh
export KELP_TOKEN="<the token used by the server>"
kelp remote set http://127.0.0.1:8080
kelp publish -m "Fix checkout timeout"

# Edit tracked files after feedback, then:
kelp publish
kelp changes
```

The first publish creates the project on the remote and the change identity. Later publications keep that identity. Unchanged work reports “already published.” Failed publications retain their exact revision and request ID locally for retry. The access token is read from the environment and is not written into workspace metadata.

Open a published change into a new directory using the change ID printed by publish:

```sh
kelp open http://127.0.0.1:8080 ../review-copy \
  --project my-project --change <change-id>
```

An open change is editable and can be republished from either workspace. Concurrent revisions are both retained. When a change has multiple heads, `open` requires an explicit `--revision <object-id>`.

`kelp change new "Next task"` starts separate work based on the current files. `kelp remote remove` returns the workspace to local-only operation without deleting checkpoints. `--json` provides machine-readable command output, and `-C PATH` selects another working directory.

## Deploy the remote

```sh
export KELP_TOKEN="$(openssl rand -hex 32)"
docker compose up --build -d
curl http://127.0.0.1:8080/healthz
```

The image runs as a non-root user; a named volume persists `/data`. For deployment on another host, place it behind your HTTPS reverse proxy. Configuration is available through `KELP_LISTEN`, `KELP_DATA_DIR`, and `KELP_TOKEN`, or the corresponding CLI options. The token grants access to all projects on this instance.

This is a **single-node SQLite remote**. Run one service instance per data directory. Stop it before copying the data directory for a simple consistent backup. SIGINT and SIGTERM drain HTTP requests before exit.

## Implemented scope

- Local initialization, explicit tracking, automatic/named checkpoints, history, and recovery into a fresh directory.
- One-command publication and revision updates, durable retries, divergent heads, and exact remote checkout.
- Content-addressed objects, project-scoped storage, integrity verification, and atomic metadata updates.
- Persistent authenticated HTTP service, container packaging, and cross-platform CI.

The experimental `kelp/0` protocol uses deterministic JSON metadata, SHA-256 IDs, and whole-file blobs. It supports regular UTF-8-path files up to 16 MiB and snapshots up to 2 MiB of metadata. Local capture scans tracked files; this is not yet the PRD's large-project implementation.

Channels/landing, review approvals, merge resolution, Git import, selective hunks, sparse fetching, native bundles, signing, symlinks, chunking, retention policies, and distributed storage are future milestones. `open` retrieves a selected change's base and result, not a full project mirror. On Unix, executable bits are retained; Windows checkouts use ordinary file permissions while preserving imported executable metadata.

## Contribute

See [CONTRIBUTING.md](CONTRIBUTING.md) for setup and checks, and [the remote API](apps/remote/README.md) for interoperability.
