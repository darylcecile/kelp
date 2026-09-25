# Kelp remote

The remote accepts immutable edit transactions. It can run as a single server, a stateless gateway, or a private storage node. Source builds and container startup commands are in [CONTRIBUTING.md](../../CONTRIBUTING.md#run-the-remote).

## Deployment modes

| Mode | Configuration | Persistent state |
| --- | --- | --- |
| Standalone | No shard list | Local content and transaction journal; v0 compatibility data |
| Storage node | `--storage-only` | Its assigned objects and journal entries |
| Gateway | `KELP_SHARDS` plus `KELP_STORAGE_TOKEN` | No project-head database; routes to storage nodes |

`KELP_TOKEN` authenticates CLI traffic at the gateway. Storage nodes use a different token, passed to gateways through `KELP_STORAGE_TOKEN`. Keep the storage API private to operators/gateways. `KELP_LISTEN` sets the bind address, and `KELP_DATA_DIR` selects storage-node/standalone persistence.

The [cluster Compose file](../../compose.cluster.yaml) runs three storage nodes with separate volumes. Multiple gateways can use the same sorted member URLs. Put public deployments behind HTTPS.

## Add capacity

Add a storage node and update the gateways' `KELP_SHARDS` lists. New writes use the expanded placement. Existing immutable data is still found on earlier owners through fallback reads, and clients rediscover journals when the layout fingerprint changes.

Update gateways to the same member list. Do not remove a nonempty storage node until its data and journal have been evacuated; automated evacuation is not implemented. The current durability profile is single-copy SQLite/WAL, not replication. Stop a node before taking a simple directory-copy backup.

## Compact a storage node

Stop the node, then run `kelp-remote --compact --data-dir PATH` with its usual token configuration. Restart the node afterward. Compaction repacks physical objects and reclaims free database pages; it does not remove transactions, journal entries, or conflicting alternatives. Gateways keep using the same object IDs.

Packs contain at most 256 objects and 8 MiB of decoded bytes. They are self-contained on that node. Objects larger than the pack limit remain independently compressed. Two decoded packs are cached per reader thread to avoid repeated decompression during nearby reads.

## Transaction API

Routes below are under `/v1/projects/{project}` and require the client bearer token.

| Method | Route | Purpose |
| --- | --- | --- |
| `PUT`, `GET` | project root | Register/discover the project and layout |
| `PUT` | `/objects/blob/{id}` | Upload hash-verified file bytes |
| `GET`, `HEAD` | `/objects/{kind}/{id}` | Retrieve/check a blob or committed transaction |
| `POST` | `/transactions` | Validate and publish an atomic batch |
| `POST` | `/sync` | Read paginated per-node journals |
| `POST` | `/objects/info` | Return logical lengths for up to 128 object IDs |
| `POST` | `/objects/download` | Download a compressed, bounded object pack |
| `POST` | `/objects/upload` | Upload a compressed pack of file objects |

A transaction contains `format`, `nonce`, `message`, and `edits`. Each edit contains `parents` (preceding transactions that wrote that path) and `value` (file metadata or a deletion). The shared Rust types define the canonical compact JSON field ordering.

File objects use raw bytes. IDs are computed as:

```text
SHA-256("kelp/0\0" + object-kind + "\0" + decimal-byte-length + "\0" + bytes)
```

The `kelp/0` prefix versions the stable object envelope, not the transaction network protocol. Transactions additionally carry `format: 1` and use object kind `transaction`.

The 0.0.3 CLI supports `clone --paths` against this same API, including 0.0.2 remotes. It retrieves complete transaction metadata but batches only selected paths' file blobs. Submitted commits retain the normal full-transaction validation rules; partial checkout is a client materialization/transfer policy, not path-level authorization.

Batch info/download bodies are `{"objects":[{"kind":"blob","id":"<hash>"}]}`. Packs use `application/x-kelp-pack`: Zstandard-compressed `KLP1` framing, with a kind byte, 64-byte ASCII hash, four-byte big-endian length, and bytes per object. Packs are limited to 128 objects and 32 MiB decoded. Each hash is verified independently. Transaction publication stays separate from file uploads.

Storage compresses beneficial objects with Zstandard level 1 while retaining their logical hashes and lengths. Existing uncompressed rows remain readable. Run matching CLI, gateway, and storage-node builds when using batch endpoints.

## Acceptance and consistency

Gateways verify file availability and path-specific parent references. The transaction's owner atomically stores its body and appends one journal row. Repeating the same ID has one logical effect. No project-wide predecessor is compared.

Sync unions journal IDs and returns one cursor per member. Clients fetch the full dependency closure before projecting file state. Independent edits compose; overlapping values remain conflicts until a resolution consumes them. A missing journal returns an unavailable error rather than a falsely complete view.

The private `/storage/projects/{project}` API exposes object storage, validated appends, and journal pages. Possession of the storage token grants operator-level access; it is not a client credential.

## Compatibility and limits

Standalone servers retain `/v0` endpoints for existing clients. v1 transaction history is separate from v0 snapshot/revision history. Existing data is not silently converted into a different causal model.

Transactions are limited to 2 MiB of metadata and files to 16 MiB. Journal pages contain up to 256 entries. Full discovery requires all configured storage nodes. The prototype has no replicated acknowledgments, automated removal/rebalancing, sparse graph index, or untrusted-peer signatures.
