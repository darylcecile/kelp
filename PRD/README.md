# Kelp: independent changes, distributed storage

**Updated:** 24 September 2026

## The goal and the primitive

Kelp versions **atomic edit transactions**. Each transaction describes what it writes and which earlier versions of those files it replaces. It is not a whole-project snapshot with a parent chosen from a single project history.

```text
Initial commit T0: api.txt + ui.txt
                  /             \
T1: update api.txt               T2: update ui.txt
depends on api.txt in T0         depends on ui.txt in T0
                  \             /
          view containing {T0, T1, T2}
          api from T1, ui from T2
```

T1 and T2 can be committed offline and pushed through different servers. Their IDs do not change. Combining them requires no new merge commit or project-head update. A snapshot is a reproducible **view of a dependency-closed transaction set**.

If two transactions replace the same file version differently, both values remain in the model. Resolving the conflict creates another transaction that explicitly consumes both alternatives. Convergence means replicas agree about the values and conflicts; it does not mean every program is correct.

This draws on change-oriented VCS and replicated-data ideas already explored by Pijul and others. The proposed contribution is the integrated user workflow, transaction contract, and partitioned service, not a claim of inventing change algebra. [Research](01-research.md).

## Familiar interface, different foundation

```sh
kelp init
kelp remote set https://code.example.com/shop   # Optional; configure once.
# Edit files.
kelp commit -m "Add checkout"
# Edit and commit more when ready.
kelp push
```

`commit` saves deliberately. `push` transfers committed transactions and leaves unfinished files alone. Local-only work requires no account or server. `clone`, `pull`, `diff`, `log`, `show`, and in-place `restore` provide the ordinary interface.

The interface does not make users create transaction IDs, start background services, or register each new file. Ignore rules control inclusion. Commits and complete saved views have stable hashes, displayed as unambiguous prefixes. Restore takes a complete saved-view hash, not a locally numbered history position.

## What the implementation must prove

| Goal | Observable proof |
| --- | --- |
| Independent work stays independent | Two clients push disjoint commits without pulling, rewriting, or contending on a project head |
| Conflicts preserve work | Both concurrent values remain available; a resolution names both parents |
| Multi-file edits are atomic | One journal record introduces all file edits; incomplete uploads introduce none |
| One project spans machines | Its content and transaction journals live on several HTTP storage nodes |
| More than one write frontend | Two stateless gateways accept work for the same project |
| Capacity can grow | A fourth node accepts new writes while earlier objects remain readable |
| Exact builds remain possible | A view ID fixes a transaction-root set and its dependency closure |

These are the acceptance tests for the architectural proof. CLI naming changes alone do not satisfy them.

## Distribution

Content and transaction records are hash-partitioned within a project. Each storage node has its own durable journal. Gateways validate transactions and route them to their owners; they hold no authoritative project-head database.

Pull unions journal entries, retrieves required ancestors, and materializes the resulting view. Journal positions are transport cursors local to a node, not a global order imposed on all work.

The distributed implementation has replicated object/journal placement, indexed path-scoped journals, and read/write fallback. Repair and node evacuation preserve immutable objects and release pins. With replication factor R, discovery tolerates up to R−1 unavailable members. Production throughput and regional failure-domain qualification still require measurement.

## Documents

| Document | Purpose |
| --- | --- |
| [1. Research](01-research.md) | Git, user friction, and prior art |
| [2. Product and workflows](02-product-and-workflows.md) | Commands and user-visible guarantees |
| [3. Transaction model](03-data-model.md) | Dependencies, atomicity, conflicts, and exact views |
| [4. Distribution](04-distribution.md) | Placement, independent journals, node growth, and failures |
| [5. Protocol](05-protocol.md) | Network contracts and compatibility |
| [6. Delivery and validation](06-delivery-and-validation.md) | Evidence, implementation limits, and remaining milestones |
| [Sources](sources.md) | Annotated research references |
