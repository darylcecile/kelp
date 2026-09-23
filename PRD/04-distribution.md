# 4. Distribution and horizontal scaling

[Overview](README.md) · [Data model](03-data-model.md) · [Protocol](05-protocol.md)

## The objective

**A logical project must not have to fit on one storage server or one developer laptop.** Adding capacity should help that same project, not only provide somewhere to put another project.

The design separates four properties often bundled into “distributed”:

| Property | What Kelp promises |
| --- | --- |
| Offline/local work | Create and inspect work using local data without a server |
| Horizontal capacity | Partition one project's content and independent records across nodes |
| Availability | Replicate durable records and route around supported failures |
| Provider independence | Export, mirror, exchange, and rehost native project data |

None implies the others. A thousand copies of a repository are not a thousand-way partition of its storage. A sharded cloud service is not necessarily usable offline.

### Terms used below

- **Shard / partition:** a slice of data assigned to a storage group.
- **Replica:** another durable copy of that slice; copying improves resilience, splitting improves capacity.
- **Quorum:** enough replicas agreeing to authorize a decision; normally a majority for metadata.
- **Authority:** the service allowed to decide a channel's accepted state; it can run on several machines.
- **Fence / epoch:** a generation number that prevents an old server from continuing to act as the current writer.
- **Pin:** a promise to retain specified data until it is explicitly released or expires.
- **Index watermark:** how far a faster, derived lookup has caught up with authoritative history.

## Reference architecture

```text
 Laptop / CI worker / authorized mirror
       │                         │
       │ proposals, queries      │ immutable bytes
       ▼                         ▼
 Stateless API gateways      Regional cache / object gateway
       │                         │
       ├── Change metadata       └── Partitioned content store
       │   shards                    hash ranges, replicated
       │   (many change IDs)
       │
       ├── Channel authority
       │   shards                    Small ordered decisions
       │   (many channel IDs)        per channel; replicated
       │
       └── Integration workers ──► CI / policy checks
                    │
                    └── Derived indexes and query workers
                        path history, blame, search, mappings
```

These are logical responsibilities. The single-machine edition runs them in one process with a transactional local database and filesystem content store. The clustered edition uses replicated metadata and distributed object storage. An independent microservice for every box is not a product requirement.

Mononoke demonstrates the value of separating immutable storage, mutable metadata, and derived data. Kelp adopts that pattern while specifying a native change-oriented and portable client contract. [A4](sources.md#a4)

## 1. Partition immutable content within a project

**Partition key:** `(storage_domain, object_hash)`.

A storage domain is the boundary for access and deduplication, usually one organization or public project group. It is not the same thing as a project or folder. Physical addresses do not appear inside revision or snapshot IDs.

A placement service maps many virtual hash ranges onto storage groups. Each group replicates its assigned ranges across failure domains. Hash distribution spreads bytes across nodes; caches handle hot objects that are read far more often than others.

Why not partition only by directory?

- A popular directory would become a hot partition.
- Renaming a directory should not require moving its entire byte history.
- Identical content can be reused across paths within the authorized domain.

Path-oriented indexes can use path ranges for efficient queries while the underlying bytes use hash placement. Computation and storage need not share the same partition key.

### Rebalancing a storage range

1. Copy its objects to the new replica group and verify hashes.
2. Catch up writes accepted during the copy.
3. Publish a new placement epoch after the new group is ready.
4. Route new writes to the current owner; old owners forward stale requests.
5. Retain old copies until reads using the previous epoch have drained.

Writers are fenced by the placement epoch: an old owner cannot claim a new durable write on behalf of a moved range. Clients see an object ID and retry token, not the topology.

Small immutable objects may be packed into bounded physical segments to avoid one storage request per tiny file. Compaction is local to segments/ranges; it must not require repacking a project's entire history.

## 2. Partition mutable records by what can change independently

| Data | Logical partition key | Required consistency |
| --- | --- | --- |
| Change heads and local-host review listing | Project + change ID | Atomic per-change update; preserve divergent heads |
| Channel tip, policy, landing outcomes | Project + channel ID | One ordered authoritative history |
| Large accepted-change membership map | Immutable map pages | Verified by entry root; changed pages written before publication |
| File-history index | Project + path/file-ID range | May lag; responses expose a watermark |
| Object bytes | Storage domain + hash | Immutable; verify hash and durability before referencing |

Do not route every project operation through one “project row.” Local checkpoints require no server interaction, and publishing a proposal must not acquire the `main` channel's lock.

An extremely active individual change can still be a hot key. It is a collaboration bottleneck, not something additional hash partitions remove. Pagination, batching, and cached immutable review events reduce its cost.

## 3. Make publication a small transaction

“Published” means the host accepted a durable proposal. “Landed” means the channel authority accepted its integration.

### Publish a proposal

The user runs one `publish` command. The client first captures an exact revision locally, creating the change identity if necessary, then performs the upload and publication steps below. Automatic workspace checkpoints stay local.

```text
Client          Content shards           Change metadata shard
  │                    │                           │
  ├── upload missing ─►│                           │
  │◄─ durable receipt ─┤                           │
  │                    │                           │
  ├── publish revision + receipt ─────────────────►│
  │                    │              verify closure and authority
  │                    │              atomically update head set
  │◄──────────────────── publication receipt ─────┤
```

**Closure** means all required objects reachable from the new records. Servers validate new objects and reuse previously validated immutable subgraphs; every tiny publish must not re-walk all history. A receipt identifies a pinned upload session and its validated roots, not merely a client-supplied list of hashes.

The closure becomes durable before the metadata references it. An upload that never publishes leaves reclaimable objects, not half a visible change.

### Land onto a channel

1. Read current channel entry E and policy version V.
2. Resolve the requested exact revisions and prerequisite closure. Newer draft heads do not silently replace requested revisions.
3. Compute candidate snapshot S outside the channel's transaction.
4. Run required checks against S and gather exact-revision review evidence.
5. Persist S, its new map pages, and the proposed channel entry's object closure.
6. Atomically compare the current entry/policy with E/V, validate the channel's recorded authorization state, then publish the new entry and landing outcome.
7. Return a receipt containing the new entry and snapshot IDs.

Review approvals presented to landing become immutable evidence in the channel's admission state. A withdrawal or permission/policy change that should block a pending landing is submitted to the same channel authority and ordered against the landing. The UI must distinguish “withdrawal pending” from “effective.” This avoids pretending an eventually replicated review list is an instantaneous authorization database.

If the channel changed at step 6, the candidate is stale. Recompute and rerun affected checks; do not stamp an old green result onto new bytes. Admission workers can form a batch of independent changes and validate their combined snapshot to reduce tip contention. A failing batch is split and tested again.

## Atomic changes across physical partitions

Suppose one change updates an API in `libs/` and its caller in `services/`, with the objects stored on different nodes.

```text
Storage group A: new library bytes ─────┐
Storage group B: new caller bytes ──────┼──► snapshot S_NEW
Storage group C: new map pages ─────────┘          │
                                                ▼
                                    main tip: E_OLD → E_NEW
```

Readers pin E_OLD or E_NEW and follow its immutable snapshot. They cannot see the new API with the old caller unless that combination was deliberately part of the accepted snapshot.

This avoids a distributed transaction over every file: immutable content is written first, and one small metadata transaction makes it visible. It does require durable, readable content and retention pins before publication. A content-addressed root alone does not guarantee availability.

**Boundary:** v1 atomicity is one channel entry in one project. A workspace may mount pinned snapshots from several projects, but that does not atomically advance those projects' own channels. Cross-project coordinated acceptance is deferred.

## What scales, and what remains serialized?

| Work | Can more machines help? | Actual limit |
| --- | --- | --- |
| Store unique bytes | Yes, partition content | Replication cost, object overhead, total storage |
| Serve immutable reads | Yes, partitions plus caches | Network egress and popular-object cache misses |
| Publish unrelated changes | Yes, change-ID partitions | Per-change hot keys and metadata service capacity |
| Calculate diffs and integration candidates | Yes, worker pool | Candidate size, data locality, overlapping work |
| Answer path-history queries | Yes, indexed partitions | Cross-range fan-out and cold historical scans |
| Advance independent channels | Yes, separate authority groups | Shared infrastructure and policy services |
| Advance one authoritative `main` | Only partly | Ordered acceptance, dependency conflicts, CI throughput |

A global total order across every draft would recreate the bottleneck we are trying to remove. Kelp only orders acceptance within a channel.

For a hot channel, use bounded batches and speculative validation of exact candidate snapshots. If demands exceed that model, independent component channels can be composed into a release snapshot. That explicitly changes the team's integration semantics; it is not transparent infinite scaling.

## Replication and regional behavior

The initial clustered profile uses three replicas across three failure zones in one region. A metadata write needs a majority. An acknowledged content write needs durable copies on at least two failure zones, with repair to the third. Metadata publication checks the corresponding content receipts.

**Regional acknowledgment:** accepted writes survive one supported node/zone failure. Loss of the whole region can lose writes not yet copied elsewhere.

**Geo-durable acknowledgment:** a later profile acknowledges only after required content and metadata are durable in a second region. This increases write latency; it is an explicit service profile, not an assumption hidden inside “replicated.”

Regional mirrors can serve immutable snapshots immediately if they have the bytes. A “latest channel” request goes to the authority or explicitly returns a cached entry with its age. It must not masquerade as a fresh authoritative answer.

A deployment may use an existing consensus-backed database instead of implementing consensus. The protocol requires its guarantees, not a home-grown implementation of a particular algorithm.

## Failure behavior

| Failure | Required outcome |
| --- | --- |
| Client disconnects after upload | Resume by object/session IDs; uploaded objects remain temporarily pinned |
| Client times out after a landing decision | Query the same request ID; return the recorded outcome, never land twice |
| One content replica fails | Read a verified replica; repair in the background |
| Region is partitioned from a metadata majority | Continue local drafts and available reads; stop authoritative writes there |
| Channel leader fails | Elect a replacement from the group; old leader cannot publish decisions without quorum |
| Index is behind accepted history | Return its watermark, wait when requested, or run an exact fallback |
| A visible snapshot's object is unavailable everywhere reachable | Report unavailable/corrupt data; do not substitute newer bytes |
| A mirror has stale channel metadata | Permit explicit pinned reads; label latest-state uncertainty |

Kelp chooses consistency over write availability for shared channel decisions during a partition. Local and peer proposals remain available. This is a deliberate split in behavior, not a way around distributed-systems limits.

## Distribution between people and providers

Project identity is independent of its current URL. A signed identity document names current authorities, public verification keys, and advertised mirrors. Its first identity must be obtained through a trusted introduction, such as a known project page or exchanged fingerprint.

```text
Maintainer laptop ─── native bundle ───► Contributor laptop
        │                                     │
        └─────────► Host A ◄───────────────────┘
                       │
                 authorized replication
                       ▼
                     Host B
```

- Peers exchange immutable objects and signed proposal/review events.
- Each provider can store only an advertised subset, but must state its coverage.
- Only the designated channel authority advances that channel. An independent host may accept proposals or run its own namespaced channel.
- Authority handoff requires a recorded final position, a signed new epoch, and fencing of the old writer before the new one serves writes.
- A full mirror includes identity history, channel entries, referenced revisions, reviews, and retained content. A developer's sparse workspace is not a disaster-recovery copy.

v1 uses explicitly configured peers and HTTPS/file bundles. Automatic global peer discovery and NAT traversal are later transport work. A blockchain is unnecessary: unrelated projects do not need a global agreement about their histories.

Radicle is relevant precedent for portable identity and signed peer collaboration, while its Git-based replication solves a different storage-partitioning problem. [D3](sources.md#d3)

## Access, caches, and deletions

Knowing an object hash is not authorization to read it. Private-object requests require project/root-scoped authorization, and gateways verify membership in the permitted object closure. A hash-only endpoint must not reveal whether another private domain has a matching file.

v1 permissions are project-level. Path-selective workspaces improve performance; they are not a path-secrecy boundary. Fine-grained path secrecy would also have to address names, hashes, history, and review metadata and is deferred.

Objects may be cached indefinitely by identity, but private delivery authorization is time-bounded. Redaction can deny future server delivery and purge managed caches; it cannot recall copies already downloaded by collaborators. This is an inherent consequence of distributed ownership.

Garbage collection traces retained channel history, live changes, retained reviews, active upload pins, and explicit archive pins. It marks against a consistent root epoch and protects newer publications with a write barrier. Deletion requires that an object remain unreachable across the collection window. Reused old objects must be pinned before a publication races with deletion.

Backups include content plus a consistent metadata checkpoint/log position. Replication handles machine loss; independent backups also protect against operator error. Restore testing is a release requirement.

## Cost model: what improvement would mean

Let:

- `H` = retained unique history bytes;
- `W` = selected workspace bytes;
- `Δ` = newly introduced file bytes and non-map metadata;
- `K` = encoded bytes of changed map pages;
- `r` = replication factor.

Expected shape, not a benchmark result:

```text
Cold workspace transfer ≈ W + selected metadata + proofs
Incremental publish     ≈ Δ + K
Stored content          ≈ r × H + indexes + compression overhead
Channel decision        ≈ small entry + evidence references
```

A full history export still costs proportionally to exported history. Initial indexing still reads the relevant data. A huge refactor still touches many paths. Chunking does not make incompressible bytes disappear.

Example: if 5,000 CI jobs each need 200 MiB, that is about 0.95 TiB delivered to workers even with perfect reuse. A regional cache can prevent that traffic repeatedly reaching the origin, but cannot eliminate the final delivery. Compare cold and warm cache cases separately.

The crucial benefit is removing unnecessary whole-project work from common operations—not claiming every operation becomes constant-time.
