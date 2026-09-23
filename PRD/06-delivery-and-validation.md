# 6. Delivery, migration, and validation

[Overview](README.md) · [Research](01-research.md) · [Sources](sources.md)

## Build the smallest useful successor first

The entire proposal is a direction, not a first-release checklist. A useful local client, a portable format, and correct project history come before a large hosted service.

| Milestone | Deliverable | Exit evidence |
| --- | --- | --- |
| M0: semantic prototype | Changes/revisions/snapshots, deterministic integration, Git importer, format vectors | Reconstruct exact bytes; preserve concurrent revisions; inspect imported history |
| M1: useful local client | Automatic and named checkpoints, workspaces, selective capture, recovery, conflicts, native bundles | Users finish the workflow study; crash/recovery tests pass |
| M2: team collaboration | One-command publish, single-host review/land, exact CI candidates, Git read bridge | Small team runs real work; retries and stale candidates behave correctly |
| M3: single-project scale-out | Partitioned content and metadata, sparse clients, regional caching | Benchmark improvements on one project; failover and GC tests pass |
| M4: provider independence | Full mirror/restore, provider handoff, portable reviews, supported rehosting | Shut down original host and recover service from verified exported state |
| Later | Geo-durable writes, virtual filesystem, automatic peer discovery, richer asset workflows | Separate requirements and evidence of demand |

M1 supports explicit offline bundle exchange. M4 means production-quality host migration and federation, including operational documentation, rather than merely copying objects in a prototype.

No calendar estimate is justified before M0 establishes merge, encoding, and import complexity. Assign a responsible owner to the client, storage, protocol, migration, and user-study workstreams before scheduling implementation.

## Git migration: preserve trust, not just file contents

### Import

```console
$ kelp import git ../shop.git --project shop
Imported commits, trees, tags, identities, and parent relationships.
Preserved original Git objects and signatures as provenance.
Created native snapshots and an ID mapping.

$ kelp import verify --source ../shop.git
Verified selected refs, parent graph, modes, and snapshot bytes.
```

Required behavior:

- Preserve the original Git commit DAG, including multiple parents and merge resolutions, in an immutable import-history graph.
- Preserve raw signed commit/tag objects. A Git signature remains a signature of the original Git bytes; it does not become a signature of a newly encoded native revision.
- Map each imported commit to its exact native snapshot. Imported commits get deterministic change identities scoped to the project and original object ID; amendments cannot reliably be inferred as one change.
- Retain author/committer distinction, timestamps, messages, executable modes, symlinks, and Git links.
- Initialize channel entries from selected Git refs. Each bootstrap entry references the corresponding import-history head; channel history traverses that graph before the native acceptance sequence.
- Preserve reachable history for the selected import refs. Unreachable objects and local reflogs require a separate explicit archival import; they are not promised by an ordinary clone.

For imported merge commits, the stored result tree is authoritative. Kelp must not recompute old merges using its own algorithm and accidentally change historical bytes. Native file identities are assigned deterministically; uncertain historical renames remain uncertain rather than inventing provenance.

### Large files and submodules

Git LFS import must obtain and verify the referenced LFS payloads, not merely import pointer text as if it were the asset. A complete mirror claim fails if required payloads are missing. An explicit incomplete import may preserve pointers and report unavailable assets.

Submodules initially become pinned external mounts retaining original URLs and commit IDs. This preserves their existing separate-project semantics. Combining them into one atomic Kelp project is a separate migration decision.

### Coexistence

```text
Git contributors ──► Git intake branch ──► imported native proposals
                                                │
                                             review/land
                                                │
Kelp contributors ───────────────────────────────┘
                                                ▼
                                      authoritative Kelp channel
                                                │
                                                ▼
                                       read-only Git projection
```

During coexistence, one system owns acceptance for a channel. Once Kelp is authoritative, legacy Git submissions go to intake refs and become proposals; the projected `main` is read-only to Git clients. This avoids two independently writable authoritative tips.

The bridge exports accepted snapshots as Git commits with a recorded native-to-Git mapping. It preserves original imported IDs where original objects can be reused; newly generated native history has new Git IDs. A landing containing several changes can export one integration commit with provenance metadata, rather than pretending its atomic acceptance was several independent events.

### What Git export loses

| Native concept | Git projection |
| --- | --- |
| Accepted channel snapshot | Commit/tree |
| Stable change identity | Trailer/notes or sidecar metadata; not a native Git guarantee |
| Revision evolution | Separate refs or native archive; ordinary branch history is insufficient |
| Structured unresolved conflicts | Native-only; do not export as accepted code |
| Portable comments and approvals | Native bundle or forge adapter |
| File identity and explicit move metadata | Sidecar metadata; ordinary Git tools infer renames |
| Native content shards | Hidden behind conversion/transfer service |

Git compatibility is an adoption tool, not the definition of Kelp's internal semantics.

### Cutover and rollback

1. Import into a shadow project and compare refs, histories, and bytes.
2. Run representative CI builds from corresponding snapshots.
3. Exercise review/landing with a pilot channel.
4. Briefly freeze writes to the migration channel, import its final delta, and record authority ownership.
5. Enable Kelp acceptance and the read-only Git projection.

Rollback freezes Kelp acceptance, exports all accepted work since cutover, verifies the Git projection, and makes Git authoritative again. Archive native reviews and revision history separately. Do not promise that rolling back to Git preserves every native feature inside Git itself.

## Product validation

Recruit at least 12 participants across occasional users, experienced Git users, maintainers, and large-repository developers. This is formative research, not a statistically representative adoption forecast.

Counterbalance task order between Kelp, participants' existing Git setup, and Jujutsu where feasible. Provide equivalent brief introductions and measure both first-use and repeat-use behavior.

| Task | Measure |
| --- | --- |
| Publish a first proposal, then revise it | Commands required, identity continuity, understanding of what was shared |
| Publish only the bug fix from mixed edits | Completion, accidental extra edits, understanding of tested bytes |
| Begin separate work while a review is open | Correct task boundary and publication destination |
| Switch tasks with unfinished work | Time, lost/recovered work, number of concepts consulted |
| Update the bottom of a three-change stack | Correct dependency revision, conflicts, reviewer continuity |
| Recover after a mistaken history operation | Recovery success without an expert, time, confidence |
| Reconcile two offline revisions of one change | Whether both contributions survive and user understands the result |
| Work offline in a selected subtree | Whether users correctly predict available/unavailable operations |
| Start and retain a local-only project | No network or credentials required for initialization, checkpoints, history, or recovery; later remote attachment does not upload |

Initial decision targets: first publication and subsequent updates each require one publish command after editing; at least 90% successful recovery of captured work; no silent contribution loss; and lower median task time than the Git baseline for revision/recovery tasks without a material regression in selective publication. Report individual failures and qualitative confusion, not just aggregate scores.

If the simpler conceptual model merely moves confusion into change/revision/snapshot distinctions, revise it before expanding the platform.

## Performance validation

All numbers below are **proposed targets**, not measured capabilities.

### Reproducible workloads

| Fixture | Shape | Why it exists |
| --- | --- | --- |
| Small | 10,000 live files, 20,000 historical commits | Avoid sacrificing ordinary local projects |
| Large source | 10 million live paths, 1 million historical revisions; active view of 10,000 files / 200 MiB | Exercise working-set isolation and path-map depth |
| Asset-heavy | 1 TiB retained assets, mixed compressibility, repeated small and full replacements | Test chunk reuse, bandwidth, and storage amplification |
| Hot collaboration | 1,000 clients; both disjoint and overlapping edits; one hot channel | Distinguish proposal throughput from integration contention |

Use deterministic generators and public repositories where licenses permit. Record exact retained byte counts, file-size distribution, rename rates, change overlap, and selected history. “Ten million files” alone is not a meaningful performance result.

### Test environment

- Client baseline: 8 CPU cores, 16 GiB RAM, local SSD.
- Storage/metadata node baseline: 16 cores, 64 GiB RAM, NVMe; report database/object-store configuration.
- Client network: 1 Gbit/s with controlled 40 ms round-trip latency.
- Storage network: 10 Gbit/s; independently report cross-zone latency.
- Scale distinct storage/metadata groups from one to two to four groups, keeping three replicas per group.
- Publish cache-cold and cache-warm results; run sustained workloads through compaction and index catch-up.

For managed backends, disclose their capacity and billable resources rather than treating unlimited object-store throughput as free.

### Gates

`p95` means 95% of observed operations finish within that duration; `p99` measures the slower tail. A warm measurement has reusable cached data, while a cold measurement starts without it.

| Metric | Initial target |
| --- | --- |
| Warm local status, 10k selected files / ≤100 changed | p95 under 200 ms with a healthy filesystem watcher |
| Capture a 1 MiB edit as a local checkpoint | p95 under 500 ms after capture begins; report watcher delay separately |
| Cold materialization of the 200 MiB selected view | p95 under 15 s in the declared environment |
| Grow unrelated live paths from 100k to 10m | Warm status/checkpoint capture under 2× latency at unchanged working set |
| Scale from one to four independent storage/metadata groups | At least 2.5× sustained disjoint-publication throughput; report resource-normalized cost |
| Hot-channel acceptance metadata overhead | p95 under 250 ms after required objects/checks are ready, at a declared offered load |
| One-zone failure | No acknowledged regional write lost; metadata writes resume within a target of 30 s |
| Rebalance one range during load | No incorrect reads; p95 publication latency below 2× its pre-move value |

For every throughput result, publish p50/p95/p99 latency, saturation point, CPU, RAM, storage, egress, request counts, and retry/revalidation rates. A system that silently queues more work has not improved throughput.

### Fair baselines

Compare against:

1. Current stable Git, recording its exact version, with appropriate sparse checkout, partial clone, filesystem monitoring, commit-graph, and maintenance settings.
2. Git with realistic hosting/caching and equivalent selected data and durability requirements.
3. Jujutsu for the local revision/recovery experience.
4. Mononoke/Sapling where a reproducible deployment is feasible; label unavailable comparisons rather than inventing them.

Measure full clone separately from selected-workspace readiness. Compare transferred bytes for equivalent usable contents. Native Kelp versus a deliberately untuned Git full clone is not sufficient evidence.

## Correctness and failure qualification

These tests address the changed guarantees, not incidental implementation details:

- Crash before/after local object durability and workspace-pointer update; recover a coherent recorded operation.
- Recover an automatic checkpoint after editing without manual version-control commands; report uncaptured edits accurately if the watcher stops.
- Fail the first publish after revision capture; retry retains the same change identity, revision, and request outcome.
- Edit during upload; the in-flight revision remains fixed and later edits remain local.
- Publish a selected checkpoint while other workspace edits exist; share only the selected state and required history, keeping automatic recovery records local.
- Kill a server after it commits a landing but before replying; retry returns the same accepted entry.
- Race two proposals and two landings; preserve both proposals and serialize acceptance correctly.
- Partition the channel authority; the minority cannot accept writes.
- Move `main` while CI runs; old check evidence cannot approve an untested candidate.
- Concurrently publish while garbage collection runs; reachable content survives.
- Move a storage range during uploads and reads; stale owners cannot acknowledge misplaced writes.
- Merge repeated lines, renames, deletes, copies, path collisions, and binary replacements against conformance fixtures.
- Revoke a pending approval through channel admission; verify the documented ordering against acceptance.
- Compare imported Git parent graphs and file bytes, including merges and tags.
- Delete rebuildable indexes; rebuild and return the same exact history answers.
- Restore a full mirror onto a fresh provider with the original provider offline.

Use a state-machine model for channel acceptance and adversarial network scheduling for publication/replication. These are stronger evidence than happy-path integration tests alone.

## Major tradeoffs and open decisions

| Issue | Proposed default | Evidence needed / next decision |
| --- | --- | --- |
| Storage efficiency versus random reads | Chunked/individually addressable objects; no required delta chains | Compare source-history storage amplification with Git packs |
| Ordered text merge versus patch algebra | Deterministic three-way merge with explicit conflicts | Prototype repeated-line and stacked-change cases; reconsider if poor |
| New client versus Jujutsu extension | New native contract; reuse compatible libraries where practical | M0 should determine whether a backend/protocol extension achieves the same goals |
| Metadata engine | Existing transactional/consensus system | Choose after transaction shape and license/operations requirements are known |
| Optional virtual filesystem | Ordinary sparse directories first | Establish whether checkout expansion is a real bottleneck |
| Permission granularity | Project-level first | Validate demand before adding path-level secrecy |
| Canonical format and chunks | Versioned profiles; provisional CBOR/SHA-256 and chunk sizes | Golden vectors, security review of formats, measured overhead |
| Retained drafts and review history | Explicit roots and visible retention | Measure storage growth and test recovery expectations |
| Authority recovery after total loss | Restore checkpoint; advance signed epoch | Specify operational ownership and old-authority fencing before M4 |

## Reasons to stop or narrow the project

- If Jujutsu plus a scalable Git-compatible backend meets the goals with less migration cost, prefer that integration over inventing another client.
- If users cannot reliably distinguish revisions from integration snapshots, redesign the workflow before adding commands.
- If one-project throughput does not improve after adding partitions, identify the serial bottleneck instead of advertising horizontal scale.
- If native storage costs materially exceed tuned Git without an accepted operational benefit, revise the storage layout.
- If provider-independent restore cannot be demonstrated, do not claim decentralization or portability is complete.

## Definition of a successful successor

A developer can save and revise work without fighting history, a reviewer can tell exactly what was approved, and a build can identify exactly what was tested. A project can outgrow one machine without changing its logical boundaries. Its owners can move it elsewhere with its history and collaboration intact.

Those are the deliverables. New terminology, a new hash, or a new hosting website alone would not satisfy them.
