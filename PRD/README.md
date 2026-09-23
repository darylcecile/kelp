# Kelp: a change-first successor to Git

- **Status:** proposal for review, not an implemented system
- **Research date:** 23 September 2026
- **Working name:** Kelp; proposed command: `kelp`

## The proposal in one minute

Kelp is a local-first version-control system built around **changes that keep their identity**, **exact snapshots that builds can reproduce**, and **storage that can split one project across many machines**.

In Git, people usually collaborate by moving branches through a history of commits. In Kelp, people edit normally, then run `kelp publish -m "Fix checkout timeout"`. That one command creates the change identity, records an exact revision, and shares it. After feedback, another `kelp publish` updates the same change without losing its identity or discussion.

```text
Edit files → Publish → Review → Land
    │                     │       │
Automatic local       Edit and    Exact snapshot
checkpoints           republish   for CI and releases
```

Underneath, immutable content is stored in independently fetchable pieces. Small records say which changes and snapshots are current. A laptop can hold everything for a small project, or only its selected working set in a huge one. A server can distribute one project's content and metadata across storage nodes.

**Local-only is a complete starting mode.** `kelp init` creates a workspace without a server, account, or network connection. Checkpoints, history, and recovery operate locally. A remote can be attached later with `kelp remote set URL`; attaching it does not upload anything.

**The primitive to replace is not hashing or snapshots. It is the coupling of a unit of work to its position in a repository-wide commit history, and of a logical project to a repository-shaped storage/transfer unit.**

## What actually changes?

| Question | Git's native model | Proposed Kelp model |
| --- | --- | --- |
| What am I working on? | A branch, working tree, index, and commits | Workspace edits; a change is named on first publish |
| How is unfinished work retained? | Explicit commits/stashes; local reflogs for reference movements | Automatic local checkpoints; optional named checkpoints |
| What happens when I revise it? | New commit IDs; external tools track continuity | Same change ID, new immutable revision |
| What gets reviewed? | Usually a host's pull request over commits | A portable review attached to an exact revision |
| What gets built? | An exact commit's tree | An exact integration snapshot |
| What if work conflicts? | Unmerged index state and operation-specific continuation | A saved conflict with named inputs; resolve when ready |
| What must I download? | Full, shallow, or partial clone, plus checkout choices | An explicit working set and requested history, with completeness reported |
| How does one large project scale? | Optimized Git plus hosting infrastructure | Independently partitioned content, change records, and indexes |
| Who decides what `main` means? | The chosen remote's branch authority | A project-designated, replicated channel authority |

These are comparisons of native abstractions, not claims that Git tooling cannot provide similar features. Jujutsu, Sapling/Mononoke, Pijul, and Radicle already demonstrate important parts of this direction. [Research and sources](01-research.md).

## The distribution answer

Git is distributed, and Git hosting **can** scale horizontally. Replicas can serve reads, different repositories can live on different nodes, and current Git supports partial clones and cache-friendly bundle delivery.

Kelp's harder target is **intra-project scaling**: adding machines should increase capacity for a single large project without requiring users to divide it into independently managed repositories.

Three things scale differently:

1. **Content:** split by content hash; replicate and cache near readers.
2. **Independent work:** partition change records by change ID; publish unrelated changes concurrently.
3. **Accepted history:** order updates per channel, such as `main`. Prepare and test in parallel, but atomically decide which snapshot becomes current.

There is no promise of unlimited parallel writes to one authoritative `main`. Agreeing on one answer requires coordination. The design keeps that coordinated step small and avoids making it copy files, calculate large diffs, or run tests. [Distribution design](04-distribution.md).

## Read by interest

| Document | What it answers |
| --- | --- |
| [1. Research](01-research.md) | How Git works, what people struggle with, and what existing successors teach us |
| [2. Product and workflows](02-product-and-workflows.md) | Who this is for, requirements, and concrete CLI examples |
| [3. Data model](03-data-model.md) | Exact meanings of changes, revisions, snapshots, conflicts, and history |
| [4. Distribution](04-distribution.md) | Partitioning, replication, atomicity, offline work, failures, and federation |
| [5. Protocol](05-protocol.md) | Client/server contracts, synchronization, transfers, and error behavior |
| [6. Delivery and validation](06-delivery-and-validation.md) | Migration, scope, milestones, benchmarks, and unresolved decisions |
| [Sources](sources.md) | Annotated evidence and limits of the research |

Start with this overview and the workflow examples. Storage reviewers should then read documents 3–5 together.

## Decisions proposed for approval

- Keep local operation, immutable snapshots, content verification, and portable history.
- Make change identity independent of revision bytes and integration position.
- Use explicit, ordinary file-level edits with recorded move information. Language-aware merging is optional future work.
- Make partial replication normal and full mirroring available; state exactly what works offline.
- Separate dissemination of proposals from authority to update shared channels.
- Retain atomic project-wide snapshots even when their data spans machines.
- Ship a useful single-machine implementation before the clustered service.

## Costs we accept

Kelp has more kinds of records than Git's compact core. Sparse clients depend on reachable providers for uncached data. A clustered installation needs real distributed-database operations. Git compatibility cannot preserve every new concept.

The proposal is justified only if those costs buy measurably easier collaboration and better single-project scaling. The [validation plan](06-delivery-and-validation.md) includes comparison with well-configured Git and Jujutsu, and explicit reasons to stop or narrow the project.
