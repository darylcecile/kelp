# 5. Transaction synchronization protocol

[Overview](README.md) · [Model](03-data-model.md) · [Distribution](04-distribution.md)

## Public API: `kelp/1`

| Method | Route | Meaning |
| --- | --- | --- |
| `PUT` | `/v1/projects/{project}` | Register a project on storage members |
| `GET` | `/v1/projects/{project}` | Discover protocol and storage layout |
| `PUT` | `/v1/projects/{project}/objects/blob/{id}` | Upload verified immutable file bytes |
| `GET`, `HEAD` | `/v1/projects/{project}/objects/{kind}/{id}` | Retrieve/check a blob or published transaction |
| `POST` | `/v1/projects/{project}/transactions` | Validate and publish one atomic transaction |
| `POST` | `/v1/projects/{project}/sync` | Read incremental journal entries from all members |
| `POST` | `/v1/projects/{project}/objects/info` | Check existence and logical sizes of up to 128 objects |
| `POST` | `/v1/projects/{project}/objects/download` | Retrieve a bounded compressed object pack |
| `POST` | `/v1/projects/{project}/objects/upload` | Upload a bounded pack of file objects |

All public project routes require the client bearer token. `/healthz` is public. There is no shared-head update route in this protocol.

## Discovery

```json
{
  "project": "shop",
  "protocol": "kelp/1",
  "layout": "<layout-fingerprint>",
  "storage_nodes": 3,
  "replicas": 2
}
```

The layout binds cursor positions to a particular ordered node list. When it changes, clients restart journal discovery. Their stored immutable transactions and file objects remain usable.

## Explicit commit, then push

`commit` persists an atomic transaction, its referenced file bytes, and a local saved snapshot. It adds the transaction ID to an outbox. `push` transfers that outbox and required dependencies in dependency order; it does not inspect current files to create a new transaction.

For each queued transaction:

1. If already published, record the acknowledgment locally.
2. Upload missing file contents.
3. Submit the exact transaction object.
4. Check the receipt's ID and remove that ID from the outbox.

A partially successful push can acknowledge some commits while others remain queued. Each commit is atomic; the whole outbox is not one atomic publication.

Push inventories the outbox and descends into ancestors only when they are absent remotely. It does not enumerate every historical file. Pull stops dependency traversal at transactions already included locally and validates only the new graph plus its boundary parents.

## Object batches

Info/download requests contain `objects: [{"kind": "blob", "id": "..."}]`; kinds are `blob` or `transaction`. Duplicate keys or batches above 128 objects are rejected. Info returns logical byte lengths or null, plus available `copies`. A publication is treated as durable only when the required replicas are present.

Packs are Zstandard-compressed binary bodies. The decoded stream starts with `KLP1`, followed by records containing a one-byte kind, 64 ASCII hash characters, a four-byte big-endian length, and exact object bytes. The total decoded limit is 32 MiB, including framing; individual blob/transaction limits still apply. Decoders enforce counts, lengths, kinds, uniqueness, and hashes before installing objects.

File uploads can be batched and hash-routed by a gateway to storage owners. Transaction **publication** still uses its own endpoint after its file closure exists. A pack is a transport optimization, not a multi-commit transaction. Deploy matching v1 CLI/gateway/storage builds for the additive batch routes.

## Transaction acceptance

The request body is the transaction described in [the model](03-data-model.md). Parent lists refer only to prior versions of the paths being written.

The gateway checks structure, format, parent membership, and replicated content closure, including chunk manifests and optional provenance. Each replica performs:

```text
begin local storage transaction
  insert immutable transaction object if absent
  insert (project, transaction ID) into journal if absent
commit
return transaction ID
```

The content ID is the idempotency key. A retry cannot attach different bytes to it. Nothing compares a repository-wide predecessor or waits for unrelated transactions on another owner.

## Incremental synchronization

```json
{"cursors": [12, 7, 19]}
```

```json
{
  "transactions": ["T_NEW_A", "T_NEW_B"],
  "cursors": [13, 8, 19],
  "more": false
}
```

An empty cursor list starts discovery. Each storage journal returns at most 256 entries per page. The gateway unions the IDs and retains each member's cursor. Clients repeat while `more` is true.

Transaction bodies may reference parents not listed in this particular page or observed journal prefix. The client fetches those parents recursively before validating/materializing the view. Duplicate and out-of-order delivery are harmless; incomplete closure is an error.

The optional `paths` field selects indexed journal entries touching those prefixes or structural ancestors. The client then obtains their complete transaction dependency closure, requests selected file blobs and provenance, and verifies hashes/lengths. Transactions are never split into per-path fragments. Expanding a selection resets cursors. Older servers can ignore the additive field and return full metadata correctly.

Cursors are saved after a successful local synchronization. A conflict that stops pull does not advance them past unincorporated work.

## Conflicts and resolution

Push can accept concurrent conflicting transactions because both are valid independent contributions. Pull identifies the resulting file alternatives. It never selects a winner by journal sequence, timestamp, or hash order.

`pull --keep-local` or `--keep-remote` selects draft file contents in an existing workspace. `commit` records the actual resolution, with every competing file parent in its edit. Until that transaction is included, the shared view remains conflicted.

The client can materialize a clean three-way text merge during clone or pull, retaining original heads until an explicit resolution commit. Overlapping selected conflicts and structural collisions still require a contributor's resolution. Unrelated outside conflicts are neither fetched nor silently resolved by a partial checkout.

## Private storage API

Storage nodes use `/storage/projects/{project}` routes for project registration, immutable objects, journal pages, and validated-transaction appends. Only gateways/operators receive the storage token.

Gateways hash-route replicated writes. Reads try the current owner, then other members. Sync tolerates fewer than R failed members, preserving their cursors; exceeding that budget returns unavailable. `/storage/drain` fences new writes during evacuation, and `/storage/inventory` pages immutable content for verified copying.

## Release pins

`POST /pins` validates a pin's complete snapshot against the dependency closure of its causal roots, then replicates it. `GET /pins?after=HASH` returns up to 128 pins in hash order. Pins are immutable typed objects; concurrent reuse of a name retains both IDs. Clients resolve unambiguous names to saved-view hashes. Pins cannot expose uncommitted working files.

## Errors

| Condition | Result |
| --- | --- |
| Missing/invalid client token | `401 UNAUTHORIZED` |
| Invalid format, hash, path, parent, or length | Request rejected; no journal entry |
| Missing dependency/content | Request rejected; local committed transaction remains queued |
| Unavailable storage/journal | `503 UNAVAILABLE` |
| Concurrent source edits | Both transactions retained; conflict reported when constructing a view |
| Unknown protocol | Client fails explicitly before creating a project directory |

## Format boundary

Transport is HTTPS in deployment, HTTP behind the local test/deployment proxy. Metadata is deterministic compact JSON; individual file endpoints use raw bytes and batch endpoints use compressed packs. The typed object-hash envelope remains versioned separately from the network API.

Standalone servers retain v0 endpoints for existing clients/data. v1 clients do not silently downgrade to snapshot-head synchronization. Existing transaction/file encodings remain valid; extended entries and provenance require transaction format 2. Signing and untrusted sparse proofs are separate from the authenticated storage protocol.
