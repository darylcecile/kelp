# 4. Distribution without a project-wide write head

[Overview](README.md) · [Transaction model](03-data-model.md) · [Protocol](05-protocol.md)

## What is distributed

One project's file objects and transaction journal entries are partitioned across storage nodes. Any configured gateway can accept its commits. Gateways have no project-head database and do not serialize unrelated writers through one project counter.

```text
Alice CLI ──► gateway A ─┬──► storage A: objects + journal
                        ├──► storage B: objects + journal
Bob CLI ────► gateway B ─┴──► storage C: objects + journal
```

Each node has an independent durable database. The same binaries can run on different machines. The container example uses separate storage containers and volumes; integration tests use separate HTTP listeners and databases.

## Placement

The preferred owner is selected by hashing `(project, object kind, object ID)` over the configured node list. Each object and transaction journal entry is replicated to R distinct nodes, beginning at that owner and continuing through the member ring. Unavailable destinations are replaced by reachable members before acknowledgment. File chunks can live on different nodes from their transaction.

Each node may losslessly pack its local objects. Pack offsets and compression relationships are physical storage details, not new transaction dependencies. The node returns the same logical bytes and hashes through the existing object endpoints, so no cross-node decoding chain or project-wide repacking lock is required.

Sorted node URLs give independently started gateways the same layout fingerprint and placement. The prototype uses hash-modulo placement; it does not claim locality-aware placement, balanced hot reads, or minimal movement on membership changes.

## Write path

```text
CLI                       gateway                  storage nodes
 │                            │                          │
 ├── missing file objects ────►├── verify hash / route ───►│
 │                            │                          │
 ├── committed transaction ───►│                          │
 │                            ├── verify parents / bytes ►│
 │                            │                          │
 │                            └── append complete batch ►│ owner
 │◄────────────────────────────── receipt = transaction ID
```

The gateway validates parent transactions and replicated file/chunk availability. Each target atomically stores the transaction and its journal row. A receipt requires R acknowledgments. Copy counts in object-info responses let retries repair incomplete replication instead of treating the first visible copy as fully durable.

There is no expected global head. Another writer adding an unrelated transaction does not invalidate this one. Overlapping writes are also retained; their conflict is part of the derived view rather than being resolved by whichever gateway received a request first.

## Why multi-file publication needs no cross-node two-phase commit here

File bytes are immutable and uploaded before publication. One transaction record contains all edits. Its journal row makes that whole record discoverable; readers obtain the complete record and its dependencies before using it.

This is an append-only, conflict-preserving model. It does **not** promise that every accepted combination is conflict-free or passes application tests. A system that instead promises one globally clean, immediately current branch would need a coordinated acceptance layer.

Immutable release pins select committed, conflict-free views without becoming the mandatory write path for independent work. Protected integration policy can build on those exact view identities.

## Read and synchronization path

Each node exposes an incremental, paginated journal. Clients keep one cursor per node. Gateways query the journals in parallel and return the union of new transaction IDs.

Journals may be observed at different moments. Before rendering, the client fetches every missing dependency. The resulting set is a **causal cut**: if a change is included, the work it depends on is included too.

```text
node A prefix ─┐
node B prefix ─┼── union IDs → fetch dependency closure → reproducible view
node C prefix ─┘
```

This is not a globally synchronized “latest instant.” It is an exact, complete set of observed work. No transaction is fractured across files, and a missing node or missing dependency is reported rather than treated as an empty answer.

## Adding capacity

A new node can be added to the gateway configuration for an existing project:

1. The project is registered on the new node when discovered through an existing member.
2. New content and transaction writes use the expanded placement.
3. Existing data can stay on its earlier owners. A miss at the preferred owner probes the other members.
4. The changed layout fingerprint causes clients to restart journal discovery, retaining and deduplicating their existing verified data.

Transaction IDs, local commit identities, and project URLs do not need to change. Operators must update the gateways to the same membership list; a gateway with an older list cannot discover the new node's journal until reconfigured.

**Node removal uses evacuation.** `--evacuate URL` persistently drains that node, walks its bounded inventory pages, and copies its objects, journals, and release pins to R remaining nodes. The operation is restartable and retains source data. Only after completion are gateways switched to the reported remaining topology. `--repair` restores placement across a supplied membership list, including migration from older single-copy stores.

## What scales, and what does not yet

| Work | Current mechanism | Remaining constraint |
| --- | --- | --- |
| Store one project's bytes | Hash-partitioned, replicated objects and chunks | Replica storage grows with the configured durability factor |
| Accept independent transactions | Independently owned journal appends | Each node still serializes its own SQLite writes |
| Add request frontends | Stateless gateways | Gateway validation can fan out to dependency owners |
| Discover new work | Parallel, path-indexed journal reads | At most R−1 members may be unavailable |
| Add storage capacity | Expanded placement plus fallback reads | Older objects may incur extra probes |
| Materialize a project | Cached file frontiers, extended from new transactions | Current map/membership processing still grows; paged indexes are future work |

Physical partitioning and independent acceptance are implemented behaviors. Linear throughput scaling, production-scale latency, and large-client performance are not measured claims.

## Durability and failures

Storage nodes commit with SQLite WAL and full synchronous durability. A receipt acknowledges R durable copies on distinct configured nodes. Geographic/failure-domain separation remains an operator deployment responsibility.

| Failure | Behavior |
| --- | --- |
| Upload stops before publication | Unreferenced blobs may remain; no partial file batch is published |
| Reply lost after acceptance | Retry the same transaction ID; one logical effect |
| A dependency is unavailable | The new transaction is not accepted |
| Up to R−1 journals are unavailable | Sync uses surviving copies and preserves missing members' cursors |
| R or more journals are unavailable | Sync fails explicitly rather than claiming completeness |
| A gateway stops | Another identically configured gateway can serve the same project |
| A storage node is permanently lost | Surviving replicas serve the data within the fault budget; repair restores placement |

Normal local commits, inspection, and recovery still work while the remote is unavailable. A push sends durable local transactions, so a failed transfer does not alter their contents.

## Trust boundary

The public API authenticates clients. Storage nodes expose a separate private API and use a separate storage token. Trusted gateways validate dependency closure before appending to storage journals. Storage credentials belong to operators, not ordinary CLI clients.

Clients verify object hashes and transaction dependencies when reading. Project-scoped namespaces prevent an object in another project from satisfying an ordinary lookup. Cryptographic author identities, signatures, fine-grained access policies, and untrusted peer federation remain future work.

## Next scaling steps

- Page the cached file frontier and membership sets to reduce current-map processing costs.
- Add regional caches; some gateway-to-storage reads still fetch individual objects with bounded concurrency.
- Bind CI results and protected integration policy to immutable release pins.
- Measure one-project throughput, cost, and tail latency against tuned Git and mature alternatives.
