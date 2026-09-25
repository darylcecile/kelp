# 1. Research: what should replace Git, and why?

[Overview](README.md) · [Next: product and workflows](02-product-and-workflows.md)

## Findings

1. Git's immutable, content-addressed storage is a strength worth retaining.
2. Snapshot identity is useful, but unrelated edits need not depend on a shared project-wide history position. File-scoped change dependencies are a candidate alternative.
3. Many everyday complaints concern confusing state and terminology. They do not establish that the storage model must be replaced.
4. Large-project scaling requires separating storage, computation, and coordinated publication. Production systems already demonstrate this separation.
5. Peer-to-peer distribution and horizontal server scaling are different problems. A successor should address both explicitly.

This is a desk-research synthesis of official documentation, engineering reports, user observations, and prior research. It is not a new representative user study. Source IDs link to the [annotated bibliography](sources.md).

## How Git actually works

### Objects describe snapshots, not a stored sequence of text patches

Git has four main object types: blobs, trees, commits, and annotated tags. Objects are immutable and addressed by a hash of their encoded type and contents. A blob stores file bytes; a tree maps names and file modes to objects; a commit names a root tree, parent commits, and metadata. [G1](sources.md#g1)

```text
HEAD ──► refs/heads/main ──► commit C
                              │
                  ┌───────────┴────────────┐
                  ▼                        ▼
               parent B                root tree
                                          │
                           ┌──────────────┴───────────┐
                           ▼                          ▼
                       README blob                  src tree
                                                      │
                                                      ▼
                                                  app.ts blob
```

Unchanged files reuse the same objects. A commit does not physically copy the entire project. Git calculates a diff when requested. Packfiles compress groups of objects and can store objects as deltas against other objects; this physical compression is separate from the logical snapshot model. [G1](sources.md#g1), [G4](sources.md#g4)

The commit hash includes its parents and metadata. Changing the parent, message, or contents creates another commit ID. Rebasing also recreates affected descendants because their parent IDs change. That is correct for immutable history, but does not natively answer: “Is this still the same reviewable change?”

### A branch is a movable name

A branch points to a commit. Usually `HEAD` points to the current branch; in detached-HEAD state it points directly to a commit. Detached work is valid, but discovering it later can require the reflog if no branch retains it. Reflogs record local reference movements, are not synchronized as normal project history, and have retention limits. [G1](sources.md#g1)

Git can therefore be very good at retaining bytes while making their whereabouts difficult for a user to understand.

### There are several versions of a file in play

```text
Working file ── git add ──► Index / staging area ── git commit ──► Commit
     │                            │                                │
what the editor sees       what would be committed          recorded snapshot
```

This supports valuable partial commits. It also means that the code tested in the working directory can differ from the code committed. During conflicts, the index can hold base and competing versions for a path. The staging area is doing several jobs at once. [G1](sources.md#g1), [U3](sources.md#u3)

**Design implication:** retain partial-change selection, but let users materialize and test the exact selected revision. Removing the word “index” without preserving this capability would be a regression.

### Fetching transfers reachable objects

In a typical fetch, the client identifies wanted commits and advertises history it already has. The server determines missing reachable objects and sends a pack. Indexes and reachability bitmaps can avoid expensive repeated walks. Push transfers objects and requests reference updates, including expected old values. Atomic multi-reference pushes are available when supported by the server. [G2](sources.md#g2), [G4](sources.md#g4), [G7](sources.md#g7)

Modern Git is materially better than a “download everything” caricature:

- **Sparse checkout:** limit which paths appear in the working directory.
- **Partial clone:** omit selected objects and fetch them on demand.
- **Shallow clone:** truncate ancestry; this changes available history and can affect merge-base discovery.
- **Protocol v2:** selective reference discovery and stateless requests suitable for load balancing.
- **Bundle and packfile URIs:** distribute prebuilt object collections through other HTTP infrastructure.
- **Commit-graph, multi-pack indexes, filesystem monitoring, and incremental maintenance:** reduce repeated local and server work.

These solve different problems and must not be treated as synonyms. A sparse checkout alone does not mean the historical object store is small. A partial clone does not guarantee every later operation works offline. [G2](sources.md#g2), [G3](sources.md#g3), [G6](sources.md#g6)

## What people complain about

The table distinguishes evidence of frustration from a proposed explanation. The quoted questions are paraphrased scenarios, not verbatim interview quotations.

| User's question | Evidence | Diagnosis | Kelp response |
| --- | --- | --- | --- |
| “Which version did I just save?” | Gitless research examines conceptual mismatches; staging has multiple roles | Working copy, index, and history require separate mental bookkeeping | Explicit versions; log descriptions and changed files; show exact diffs |
| “Why is my branch up to date if push fails?” | Evans documents stale remote-tracking information and confusing push/pull advice | Last-observed remote state looks like current remote truth | Distinguish the last shared version from the remote's current head |
| “Which side is mine in this conflict?” | Evans documents shifting `ours`/`theirs` meanings across operations | Operation-dependent labels hide actual inputs | Label conflict sides by author, revision, and target snapshot |
| “How do I get my work back?” | Reflog documentation; self-reported losses in Evans's polls | Recovery exists, but is not one discoverable workflow and cannot recover every unsaved file | In-place restore with a labelled, inspectable backup |
| “Why did a small edit disrupt my review stack?” | Jujutsu's design addresses revision identity and descendant rewrites | Commit identity also encodes ancestry; review identity is external | Stable review identity in an optional collaboration layer |
| “Why does checkout require so much data?” | Git partial-clone design; Sapling scale documentation | Working set, historical data, and transfer planning are separate concerns | Select paths and history explicitly; batch object retrieval |
| “Why do large assets need another system?” | Git LFS stores pointers separately from large-file contents | Large-file distribution is an extension with another storage lifecycle | One content model for source and assets, with chunked large files |
| “Why is our giant repository a special infrastructure project?” | GitHub maintenance report; Gitaly limitations; Mononoke architecture | Repository-wide computation and placement can concentrate work | Partition data within one project; keep expensive work outside publication |

Sources: [U1–U3](sources.md#u1), [A1–A4](sources.md#a1), [G3–G6](sources.md#g3), [D1](sources.md#d1), [G8](sources.md#g8).

### How strong is this evidence?

Evans's March 2024 article reports that 17% of 1,475 respondents said they had lost work because of a Git problem in the preceding year or two. The same article explicitly calls its polls highly unscientific: the audience is self-selected, phrasing varies, and responses are not independently verified. This is a reason to study recovery, not an estimate of Git's data-loss rate. [U1](sources.md#u1)

The Gitless work supplies a conceptual analysis and a small user study, but dates from 2013/2016. It predates later Git UX improvements. We use it to identify testable design hypotheses, not to declare today's Git universally unusable. [U3](sources.md#u3)

People also value Git's index, flexible workflows, local speed, and ecosystem. The proposal must preserve those capabilities or demonstrate a better way to accomplish the same tasks.

## Is Git horizontally scalable?

**Yes in several senses; not automatically in every sense.** “Git is distributed” describes independent repositories exchanging history. It does not itself specify a cluster that divides one repository's workload among servers.

| Scaling dimension | What already works | Remaining pressure |
| --- | --- | --- |
| Many independent repositories | Place repositories on different servers | One exceptionally large repository can dominate its placement |
| Many readers of one repository | Replicas, caches, reusable bundles, object sharing | Cold data, pack computation, and replica freshness can concentrate load |
| One huge working directory | Sparse checkout, filesystem monitoring, optional virtualization | Tools that insist on scanning every file still cost proportionally |
| One huge object/history store | Packs, indexes, incremental maintenance, partial clones | Not a native general-purpose distributed storage engine |
| Many independent proposals | Separate refs and hosting workflows | Repository-level processing can still be expensive |
| Many updates to one branch | Serialized ref updates and integration queues | One authoritative tip requires agreement on ordering |

Gitaly Cluster distributes reads across eligible replicas. Its documentation specifically notes that, for a heavily modified large repository, replication lag can cause the primary to serve most or all reads. That is a concrete hosting bottleneck, not proof that Git's object hashes cannot be partitioned. [D1](sources.md#d1)

Mononoke is stronger counter-evidence to the absolute premise: it separates immutable blobs, mutable metadata, and derived indexes, uses external distributed storage, and serves Git as well as Sapling. A horizontally scalable backend does not inherently require abandoning the Git protocol. [A4](sources.md#a4)

**Our conclusion:** a clean successor can make these capabilities native and interoperable. It cannot honestly claim to have invented scale-out source control.

## Prior art and what to borrow

| System | Useful idea | Boundary of the lesson |
| --- | --- | --- |
| Jujutsu | Change-oriented UX, first-class conflicts, operation log, no user-facing index | Git-backed use alone does not supply Kelp's proposed clustered storage contract |
| Sapling + Mononoke + EdenFS | Working-set-scaled clients; separate content/metadata; asynchronous derived data | Large-scale service operations are substantial; Sapling documents a move toward client-server operation |
| Pijul / Darcs lineage | Changes and dependencies are primary; conflicts can be represented as data | A mathematical convergence property is not proof of semantic correctness or deployment-scale performance |
| Google Piper + CitC | One logical codebase with distributed infrastructure and small workspace overlays | Published experience is tied to Google's environment and tooling |
| Radicle | Portable identity, signed collaboration records, peer discovery and replication | Peer-to-peer Git distribution is not the same as splitting one repository across storage shards |
| Gitless | Reduce mismatches between tasks and exposed state | Simplification must be tested against expert as well as beginner workflows |

Sources: [A1–A6](sources.md#a1), [D2–D3](sources.md#d2), [U3](sources.md#u3).

## Why not choose a more radical primitive?

### Pure text-edit CRDT

A **CRDT** is a data structure whose replicas can combine concurrent updates according to defined rules. Convergence means replicas agree; it does not mean the resulting program is correct. Pijul explicitly distinguishes a conflict-free replicated representation from the source conflicts that representation contains. [A5](sources.md#a5)

Kelp's implemented transaction model is a replicated set of immutable, file-scoped edits. Set union is order-independent; competing file values remain explicit conflicts. It does not silently turn convergence into a claim of correct code. Character-level editing would additionally need decisions about deletion history, storage growth, and binary files.

### Syntax-tree or AI-native history

Moving a function can be clearer than “delete these lines, add those lines.” However, a universal syntax model must handle generated files, broken intermediate code, multiple languages, configuration, and binary assets. It also ties history interpretation to parser versions.

Kelp stores ordinary bytes and explicit file operations. Language tools may suggest a merge, but acceptance records the resulting bytes. An AI suggestion has the same status as any other unreviewed edit.

### Just put Git objects in object storage

This can help hosting, and existing systems do related work. It does not alone provide a simpler local workflow, predictable partial-workspace behavior, or a defined cross-shard publication transaction.

### Completely remove snapshots

Builds, releases, debugging, and incident response need an unambiguous answer to “Which bytes were used?” Kelp derives exact snapshots from selected transaction sets and keeps local materialized views for recovery. Efficient persistent view indexes are still needed before large-history performance can be claimed.

## Research-derived design boundary

Kelp uses **atomic file-scoped transactions over partitioned content and journals**. Independent transactions compose without creating another project-wide merge commit. The next documents specify that contract, the multi-node proof, and the remaining limits. Familiar commands are an interface constraint; they are not the architectural differentiator.
