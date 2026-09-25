# Kelp remote

The remote accepts immutable edit transactions. It can run as a single server, a stateless gateway, or a private storage node. Source builds and container startup commands are in [CONTRIBUTING.md](../../CONTRIBUTING.md#run-the-remote).

To host a remote on Cloudflare instead, use [remote-cloudflare](../remote-cloudflare/README.md), which stores objects in R2 and journals in SQLite Durable Objects.

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

Update gateways to the same member list and replica count. Existing immutable hashes are unchanged by placement or membership changes.

## Replication and node evacuation

Distributed gateways use `KELP_REPLICAS=2` by default. Each acknowledged object, transaction journal entry, and release pin is written to that many independent storage nodes. Placement starts at the hash owner and uses successive nodes, falling back to other members when a write fails. With three nodes and two replicas, one unavailable node still permits reads and writes. Sync requires at least `node_count - replicas + 1` reachable journals; it never treats too many missing journals as an empty project. A standalone server remains a single storage deployment.

For an existing single-copy cluster, upgrade the storage nodes, keep all original nodes reachable, and run replica repair before starting gateways with the stronger profile:

```sh
kelp-remote --repair --shards http://store-a:8080,http://store-b:8080,http://store-c:8080 --replicas 2
```

Use the usual `KELP_TOKEN` and `KELP_STORAGE_TOKEN` environment variables. Repair copies existing objects, journals, and pins into the requested placement; it also works after adding nodes or replacing a lost replica.

To retire a reachable node:

```sh
kelp-remote --evacuate http://store-a:8080 \
  --shards http://store-a:8080,http://store-b:8080,http://store-c:8080,http://store-d:8080 \
  --replicas 2
```

The command persistently drains writes on that node, copies its inventory to the remaining members, and waits for replicated acknowledgments. It prints the remaining node list when complete. Update every gateway to that list, then stop the retired node. Its original data is retained. If interrupted, rerun the same command; object and journal publication are idempotent. A permanently lost node can be removed from the list and surviving replicas repaired, provided the configured failure tolerance was not exceeded.

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
| `GET`, `POST` | `/pins` | List or publish immutable committed release views |

A transaction contains `format`, `nonce`, `message`, and `edits`. Each edit contains `parents` (preceding transactions that wrote that path) and `value` (file metadata or a deletion). The shared Rust types define the canonical compact JSON field ordering.

File objects use raw bytes. IDs are computed as:

```text
SHA-256("kelp/0\0" + object-kind + "\0" + decimal-byte-length + "\0" + bytes)
```

The `kelp/0` prefix versions the stable object envelope, not the transaction network protocol. Transactions use object kind `transaction`: format 1 retains the original file encoding; format 2 permits chunked content, symlinks, and optional source provenance. File-entry `kind` is omitted for ordinary files. Chunk manifests contain bounded child entries and are themselves hash-verified blobs.

`sync` accepts a `paths` selection alongside cursors. Indexed journals return transactions touching selected paths or structural ancestors; clients fetch their complete dependency closure. Unrelated history and excluded file blobs stay remote. Expanding a checkout restarts discovery for the larger selection without changing any transaction IDs. Older remotes that ignore `paths` still provide correct full metadata.

Batch info/download bodies are `{"objects":[{"kind":"blob","id":"<hash>"}]}`. Packs use `application/x-kelp-pack`: Zstandard-compressed `KLP1` framing, with a kind byte, 64-byte ASCII hash, four-byte big-endian length, and bytes per object. Packs are limited to 128 objects and 32 MiB decoded. Each hash is verified independently. Transaction publication stays separate from file uploads.

Storage compresses beneficial objects with Zstandard level 1 while retaining their logical hashes and lengths. Existing uncompressed rows remain readable. Run matching CLI, gateway, and storage-node builds when using batch endpoints.

## Acceptance and consistency

Gateways verify replicated file/chunk availability and path-specific parent references before publishing. Each replica atomically stores the complete transaction body and its journal row. A receipt is returned only after the required copies acknowledge. Object-info replies include available copy counts, so a retry repairs incomplete replication before treating a transaction as published. No project-wide predecessor is compared.

Sync unions journal IDs and returns one cursor per member. Cursors for unavailable members remain unchanged, and duplicate entries from replicas are harmless. Clients fetch full transaction dependencies before projecting file state. Independent edits compose; overlapping values remain conflicts until a resolution consumes them.

The private `/storage/projects/{project}` API exposes object storage, validated appends, scoped sync, pins, and journal pages. `/storage/drain` and paginated `/storage/inventory` support evacuation. Possession of the storage token grants operator-level access; it is not a client credential.

## Compatibility and limits

Standalone servers retain `/v0` endpoints for existing clients. v1 transaction history is separate from v0 snapshot/revision history. Existing data is not silently converted into a different causal model.

Transactions retain a 2 MiB metadata budget; each blob is at most 16 MiB, with larger files streamed through bounded chunk trees. Journal pages contain up to 256 entries. Run matching gateway/storage builds for scoped sync, replicated acknowledgments, and pins. Replica durability depends on independent persistent storage; untrusted-peer signatures and geographic failure-domain placement are separate from this replication profile.
