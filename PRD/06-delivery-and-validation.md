# 6. Delivery and validation

[Overview](README.md) · [Research](01-research.md) · [Sources](sources.md)

## Architectural acceptance comes first

The implementation is useful only if it demonstrates a different foundation, not just a more pleasant Git-shaped interface.

| Proof | Implemented test location |
| --- | --- |
| Independent edits have only file-scoped dependencies and converge in either order | `crates/kelp-core/tests/transactions.rs` |
| Concurrent values/deletions remain recoverable until explicitly resolved | Core transaction tests and CLI workflow tests |
| A dependent transaction cannot produce a partial view without its multi-file parent | Core transaction tests |
| Two contributors push without pulling/rebasing each other's independent work | `apps/cli/tests/workflows.rs` |
| Push transmits committed bytes, leaving later edits local | CLI workflow tests |
| One project occupies three independent HTTP storage nodes | `apps/cli/tests/distributed.rs` |
| Two gateways write concurrently and a fresh gateway reconstructs the project | Distributed integration tests |
| A fourth node accepts new writes while existing data remains accessible | Distributed integration tests |
| Incomplete batches are invisible; duplicate publication has one effect | Distributed integration tests |
| An unavailable journal is not silently treated as an empty partition | Distributed integration tests |

These tests exercise actual HTTP requests and separate databases. The container example additionally runs the gateway and storage nodes as separate processes with separate volumes. Placement counts are evidence of physical partitioning, not a throughput benchmark.

### Recorded verification — 24 September 2026

- The workspace suite passed 17 tests on the development machine; formatting and Clippy passed.
- The HTTP integration fixture stored 40 transactions for one project across three journals. One run reported `(journal entries, blobs)` as `(14, 11)`, `(12, 13)`, and `(14, 16)`; UUID-based commits can change the exact distribution between runs.
- A separate Linux-container run used three storage volumes and two gateways. Six committed transactions were distributed as `2 / 1 / 3` journal entries.
- Restarting all three storage containers and connecting through a fresh gateway reproduced the original view ID.
- Adding a fourth container preserved the original view and accepted a transaction on the new storage node.
- The container client left an uncommitted file local; a fresh clone did not receive it.

These results establish the primitive and physical-distribution proof. They do not establish production durability or linear throughput scaling.

## Delivery order

1. **Transaction semantics:** file parents, tombstones, dependency closure, conflicts, and deterministic views.
2. **Usable local client:** explicit commits, a durable outbox, inspectable history, and in-place recovery.
3. **Distributed proof:** partition content and journals, run interchangeable gateways, and add capacity to one project.
4. **Production qualification:** replication, node evacuation, indexed file frontiers, bounded bulk transfer, and load/fault testing.
5. **Adoption:** Git import/export, richer conflict tooling, pinned release views, selective workspaces, and portable collaboration metadata.

The first three are the current implementation scope. The remaining stages must not be described as already delivered.

## User validation

Use at least 12 participants spanning occasional users, experienced Git users, and maintainers. Counterbalance task order against their ordinary Git workflow. This is formative usability work, not a representative market survey.

Tasks: start locally; commit with a meaningful description; configure a remote once; push committed work; inspect file changes; collaborate on different files; resolve an overlapping edit; restore and recover the backup.

Measure required commands, completion time, errors, and whether users correctly predict local versus shared state. Independent collaboration should not require a pull/rebase loop. At least 90% should complete recovery without expert help; silent contribution loss blocks release.

## Performance measurements still required

Local status/commit use cached current file frontiers; pull validates new transactions and stops at known dependency boundaries. Membership IDs and current maps are still loaded, and clone still downloads shared history. Those costs must be measured before claiming support for massive histories.

Reproducible local latency/storage and request-count benchmarks are in [benchmarks](../benchmarks/README.md). Raw results distinguish before/after builds, platform, fixture size, warm-cache conditions, and tuning. They are not substitutes for the production-scale experiments below.

The hash/index/batching update passes 24 tests locally, including hash-prefix ambiguity, in-place hash restoration, cache reconstruction, same-size file rewrites, compressed/legacy object reads, and batched transfer. On the 1,000-file/100-edit fixture, warm status fell from 198 ms to 16.5 ms and incremental commit from 163 ms to 30.4 ms. Git measured 13.1 ms status and 37.9 ms add+commit. Initial Kelp capture remained much slower, and its stored metadata remained larger. The benchmark report includes these unsuccessful comparisons as well as the gains.

| Fixture | Shape |
| --- | --- |
| Small | 10,000 live files and 20,000 transactions |
| Large source | 10 million paths, 1 million transactions, 10,000-file active view |
| Asset-heavy | 1 TiB retained bytes with mixed compressibility |
| Independent writers | 1,000 clients with disjoint file sets in one project |
| Overlapping writers | Controlled conflict rates and resolution sizes |

Compare one, two, and four storage groups at equal hardware and durability settings. Record accepted transactions/s, bytes/s, p50/p95/p99 latency, per-node storage, CPU, memory, network traffic, and dependency-query fan-out. Separate initial discovery from incremental sync and cold reads from cache hits.

Do not compare a single-copy Kelp shard with a replicated Git host and call the difference an efficiency win. Use tuned Git, realistic caching, Jujutsu for local UX, and Sapling/Mononoke where deployment comparisons are reproducible.

## Failure qualification

- Interrupt uploads before the journal append: no partial transaction becomes visible.
- Lose replies after append: retry the same ID and preserve one logical effect.
- Restart storage nodes from their durable volumes and reconstruct views through a new gateway.
- Interrupt one journal during sync: return incomplete/unavailable, not a false current view.
- Add storage members: reset transport cursors and retain original transaction IDs.
- Validate parent path references, hashes, lengths, and graph closure on both write/read paths.
- Preserve current files and recovery snapshots when a filesystem update cannot complete.

Replication and node-loss recovery need additional fault models. Current single-copy durability is not protection against permanent storage-node loss.

Storage compaction qualification additionally checks every preserved object hash, conflicting file alternatives, in-place recovery backups, replay after restart, writes after compaction, older database migration, and reads through packed distributed storage. Corrupt input must abort the location transaction without removing other objects. Large objects stay independently readable rather than exceeding the decoded pack budget.

The compaction implementation passes the 30-test workspace suite plus the added incompressible-group budget test. Its maintained footprint on the 1,000-file/101-commit fixture is 323,584 bytes, compared with Git's 330,148 bytes. Kelp retains all logical objects and recovery events; GC only changes physical representation. Post-maintenance status is 20.6 ms median versus 18.0 ms before maintenance; historical show is 12.8 ms. The [raw measurement](../benchmarks/results/compacted.json) and [method](../benchmarks/README.md) record the tradeoffs and limit the size claim to this fixture.

## Compatibility and adoption

Existing local saved snapshots remain recoverable during schema upgrade; old metadata is archived. Recommit the desired state to enter transaction synchronization. Standalone v0 endpoints remain available for existing clients, but v0 history is not automatically promoted into v1 transactions.

Git import must preserve exact historical trees, merge results, author/committer metadata, signatures as original-object provenance, modes, tags, and parent relationships. A conversion should represent edits as transactions while retaining original Git history as provenance; it must not invent semantic independence that the import cannot establish.

Git LFS payloads must be fetched before claiming a complete import. Submodules initially remain pinned external projects. A Git projection can expose selected, conflict-free Kelp views as conventional commits for existing tools. Portable native archives are needed for transaction alternatives that ordinary Git history cannot express faithfully.

## Remaining product decisions

- How users select/pin historical shared views and releases.
- Richer conflict browsing for new contributors cloning a conflicted project.
- Efficient indexed frontiers and paged edit sets for very large commits/history.
- Replication, live node evacuation, and explicit storage durability profiles.
- Author identity, signatures, access boundaries, and untrusted peer exchange.
- Review and CI policy on chosen views without imposing one mandatory draft-write head.

The stopping criterion for this phase is a tested transaction primitive and a working, inspectable multi-node proof. Performance and operational claims require their own measurements rather than being inferred from the architecture.
