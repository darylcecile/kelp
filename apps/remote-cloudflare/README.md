# Kelp remote on Cloudflare

A self-contained Cloudflare Worker implementing the `kelp/1` remote API for the existing Kelp CLI. Files, chunks, transaction bodies, and release pins live in R2. Each project's journal is hash-partitioned across 16 SQLite Durable Objects.

## Deploy

You need Node.js 24+ and a Cloudflare account with R2 and Workers Paid enabled. The paid Workers request budget accommodates Kelp's object batches and validation work.

From the repository root:

```sh
npm ci
cd apps/remote-cloudflare
npx wrangler login
npx wrangler r2 bucket create kelp-objects
npx wrangler secret put KELP_TOKEN
npm run deploy
```

Choose a random token when prompted. Store the same value in the CLI's `KELP_TOKEN` environment variable. The token grants access to every project on this remote, matching the self-hosted remote's authentication model.

Wrangler prints the deployed URL. Add a project name to it:

```sh
export KELP_TOKEN='your-token'
kelp remote set https://kelp-remote.YOUR-SUBDOMAIN.workers.dev/my-project
kelp push

# On another machine:
kelp clone https://kelp-remote.YOUR-SUBDOMAIN.workers.dev/my-project
```

Change `name` and `r2_buckets[].bucket_name` in `wrangler.jsonc` if deploying another independent remote. Keep the Worker identity, journal binding/migrations, bucket, and shard count stable for an existing deployment: journals and objects together form the remote's persistent state.

## Local development

See [CONTRIBUTING.md](../../CONTRIBUTING.md#cloudflare-remote) for local startup and checks. Wrangler emulates R2 and Durable Objects; local development does not access your deployed data.

## Behavior

- Supports normal push/clone/pull, compressed object batches, chunked large files, symlinks, native path metadata, partial checkout/expansion, conflict retention, and release tags.
- Verifies object hashes, file lengths, chunk manifests, and path-specific transaction parents before publication. A journal receipt exposes the complete transaction only after its immutable payload is stored.
- Publication is idempotent. Retrying a failed upload or lost response retains the original transaction ID.
- Keeps path indexes and per-shard cursors for incremental partial synchronization. Independent transactions do not update a global project head.
- Uses Cloudflare's managed R2 and Durable Object durability. The API reports one logical copy; it does not claim the Rust remote's independently configured replica count or expose its node-maintenance commands.
- Supports 128 objects / 32 MiB decoded per batch, 16 MiB per blob, and 2 MiB transaction or release-pin metadata. Large files use chunk trees. Release-pin validation walks the pinned history and remains subject to Workers request limits.

`GET /healthz` is public; project endpoints require the bearer token. This service supports the current `/v1` API, not the legacy `/v0` API.

Platform references: [R2 bindings](https://developers.cloudflare.com/r2/api/workers/workers-api-reference/), [SQLite Durable Objects](https://developers.cloudflare.com/durable-objects/api/sqlite-storage-api/), and [Workers limits](https://developers.cloudflare.com/workers/platform/limits/).
