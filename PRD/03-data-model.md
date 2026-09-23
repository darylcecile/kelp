# 3. Data model and correctness rules

[Overview](README.md) · [Distribution](04-distribution.md) · [Protocol](05-protocol.md)

This document defines the proposed v1 semantics. **MUST** means a required compatibility rule. Encoding details marked provisional must be settled before independent implementations can claim wire compatibility.

## Separate three questions

```text
What work is this?        Change ID        C7K2
Which version of it?     Revision ID      R8A31
Which project bytes?     Snapshot ID      S92BC
```

A change ID is stable. A revision and snapshot ID identify immutable objects. None can substitute for the others.

A review follows the change but approves a revision. CI validates an integration snapshot. A channel entry records how approved work became that snapshot.

## Local checkpoints and publish-time revisions

A workspace starts with a base snapshot and an unnamed local task. Automatic checkpoints record its on-disk state in the local operation history. They do not create a public change identity or a sequence of review revisions.

A locally initialized workspace starts from an empty base snapshot and stores its captured objects locally. Its remote configuration is optional and separate from object identity. Configuring or removing a remote changes neither checkpoint contents nor the active change's identity.

`publish` captures the selected workspace state, allocates a change identity if the task has none, and durably records an exact revision before uploading it. The workspace retains that identity for later publications. Explicit dependency operations and offline bundle creation can also allocate identities locally when needed; none requires a server-issued ID.

An explicit `checkpoint` records a named, pinned local snapshot. A selective checkpoint also retains its task/base context so `publish --checkpoint` can produce exactly that proposal. A checkpoint label describes a local stopping point; the change title describes the work being shared.

Automatic checkpoint records are not revision predecessors and are not included in publication or proposal bundles. Revisions reference the selected result and required history, not discarded intermediate workspace states. The local publication record binds the task, revision, and request ID so retrying a failed first publish cannot create another change accidentally.

## Object relationships

```text
Change C7K2 ──► known revision heads {R2, R3}
                  │                    │
                  └──────► R1 ◄────────┘  revision predecessors

Revision R3
  ├── change ID: C7K2
  ├── base snapshot ──► path-map root
  ├── result snapshot ──► path-map root
  ├── requirements: exact revisions of other changes
  └── edit metadata: moves, copies, touched paths

Channel main ──► accepted entry E1848
                  ├── previous entry E1847
                  ├── accepted revisions {C7K2: R3}
                  ├── resulting snapshot S92BC
                  ├── cumulative accepted-change map
                  └── review/check evidence

Path-map leaves ──► file manifests ──► content chunks
```

Arrows between immutable objects refer to hashes. The mutable records are small: current change heads, channel tips, workspace state, and retention roots.

**Snapshot objects contain project contents, not channel history.** Two channel entries may have identical snapshot IDs while recording different provenance. This avoids making file-state identity depend on integration order or timestamps.

## Record catalog

| Record | Minimum contents | Identity |
| --- | --- | --- |
| Project identity | Initial owner keys, random project nonce, initial protocol profile | Hash of genesis identity; ownership updates form a signed chain |
| Change identity | Project ID, creator key ID, random nonce | Hash of this creation record; can be allocated offline |
| Revision | Change ID, predecessors, base/result snapshots, requirements, edit metadata, author, message | Hash of canonical revision bytes |
| Snapshot | Versioned path-map root | Hash of canonical snapshot bytes |
| Path entry | Path, file ID, file type/mode, file manifest or conflict ID | Included in a persistent path map |
| File manifest | Byte length, ordered chunk IDs/lengths, content checksum | Hash of manifest bytes |
| Conflict | Type, exact base/side inputs, provenance labels | Hash of conflict bytes |
| Review event | Change ID, exact revision, comment/approval, actor, predecessor events | Hash, with detached author signature |
| Channel entry | Channel identity, sequence, previous entry, accepted revisions/map, result, evidence, policy version | Hash, with authority signature |
| Release tag | Label, exact channel entry/snapshot, creator | Immutable signed record; a name cannot be silently retargeted |
| Import-history node | Original VCS object, ordered parent nodes, native snapshot, provenance | Hash; preserves the imported graph |
| Workspace operation | Prior operation, before/after workspace roots, task/base context, optional checkpoint snapshot/label | Local immutable record |

Signatures are separate envelopes over object ID, project scope, and action. Different signatures MUST NOT change the signed object's identity. A valid signature proves a key authorized bytes; it does not prove the code is correct or that the key is currently allowed to land it.

## Revision schema

Illustrative JSON; the canonical binary format is specified separately below.

```json
{
  "type": "revision/v1",
  "project": "P_SHOP",
  "change": "C7K2",
  "predecessors": ["R_PREVIOUS"],
  "base_snapshot": "S_BASE",
  "result_snapshot": "S_PROPOSED",
  "requires": [{"change": "C6J1", "revision": "R_API"}],
  "edits": [{
    "file": "F_RETRY",
    "before_path": "services/checkout/retry.ts",
    "after_path": "services/checkout/retry.ts",
    "before_manifest": "M_OLD",
    "after_manifest": "M_NEW"
  }],
  "author": "K_MAYA",
  "message": "Fix checkout timeout"
}
```

Rules:

1. Predecessors MUST belong to the same change and form an acyclic revision history.
2. Requirements MUST name exact revisions, form an acyclic dependency graph, and be available before the revision is advertised as complete.
3. The edit list MUST agree with the difference between base and result. A server verifies it using changed map pages and reused verified subtrees; it must not trust a client's touched-path claim.
4. A revision may contain conflict entries. A channel's accepted snapshot MUST NOT.
5. Editing author metadata, message, requirements, base, or bytes produces a new revision ID.
6. Change IDs are scoped to a project. Copying work into another project creates a new identity with explicit origin provenance.

A base snapshot can itself be a draft result. This permits stacks without forcing every prerequisite through `main` first. Bytes outside the selected workspace remain represented by existing immutable map nodes.

## Change heads and simultaneous work

The published head set is a **multi-value register**: it can hold more than one current answer rather than picking a winner by timestamp.

A **head** is a revision not superseded by another known revision. The set of heads is also called the revision **frontier**.

When R2 supersedes observed R1, publication adds R2 and removes R1 from the current head set. It does not delete R1's object or review history. If R3 independently supersedes R1, both R2 and R3 remain heads. A later revision can explicitly supersede both.

Advertisements contain immutable predecessor relationships, so replicas can calculate the same frontier after receiving the same authorized revisions. Head removal is justified by those relationships, not by an unqualified “set latest” request.

Abandoning a change is an explicit signed event that changes its discoverability. It is not permission to delete another collaborator's unpublished local work.

## What happens to cherry-pick, rebase, and revert?

### Apply a change somewhere else

Given revision R, base B, proposed result P, and target T:

```text
integration(R, T) = three-way-merge(B, P, T)
```

The result is either a candidate snapshot or a saved conflict. The integration record retains R and T. R does not change simply because it was cleanly integrated into a different target.

This is **not** a claim that arbitrary text edits commute. If applying A then B differs from applying B then A, they are different integration candidates and require validation as such.

### Update the change itself

`change update --onto` makes a new revision with a new base and rebased result, under the same change ID. Its predecessor records the connection. This differs from landing an unchanged revision into a newer target.

### Revert accepted work

A revert is a new change applying a reverse edit against the current target. Later edits can make it conflict. It never rewinds a channel tip or deletes the original acceptance record.

### Backport

An unmodified revision can be integrated into another channel. An adapted backport is a new change with `derived_from` provenance. This prevents two intentionally different implementations from appearing to be unresolved revisions of one task.

Within a channel's ancestry, a change can be accepted once. A later correction is a new change. The accepted-change map supports duplicate detection and prerequisite checks; it records historical acceptance, not a guarantee that later code still implements the prerequisite's behavior.

## File identity and conflict behavior

Each file has a stable file ID allocated at creation. Moving it retains that ID; copying it creates a new ID with copy provenance. Deleting and recreating the same path normally creates a new file.

Explicit moves help distinguish these cases:

```text
Maya: move F1 from a.ts to b.ts
Leo:  edit F1 at a.ts
Result: edit follows F1 to b.ts, if its content merge is clean
```

| Situation | v1 behavior |
| --- | --- |
| Same file, nonoverlapping text edits | Deterministic three-way text merge |
| Same file, incompatible text edits | Content conflict retaining base and both sides |
| Move on one side, edit on the other | Combine identity-preserving move and content merge |
| Same file moved to two different paths | Name conflict |
| Two files claim the same path | Path conflict; do not silently overwrite |
| Delete versus modify | Explicit delete/modify conflict |
| Competing binary replacements | Binary conflict; choose or upload a resolution |
| File versus directory at the same path | Structural conflict |

The merge algorithm and version are recorded with a candidate. The reference text algorithm requires deterministic hunk matching and tie-breaking; a conformance corpus must settle repeated-line ambiguities before v1 freezes. Syntax-aware drivers may propose bytes but cannot silently reinterpret already accepted snapshots.

A resolution records the conflict ID and replacement bytes. Reusing it automatically requires identical input object IDs and merge profile; otherwise it is a suggestion. “No textual conflict” never means “no behavioral regression.”

## Snapshot structure and large files

The path map is an immutable, paged, ordered tree. Each internal page names child pages by hash. Updating a few paths copies only the pages on those lookup paths; unchanged pages remain shared.

This is a **Merkle structure**: changing a leaf changes the hashes above it. Readers can verify selected files against a root without downloading unrelated file contents. A root is small even when the project spans thousands of storage partitions.

Provisional v1 layout:

- Path-map pages target 16–64 KiB; exact canonical page construction needs conformance vectors.
- Small files use one content object. Files above 1 MiB use bounded content-defined chunks, provisionally 256 KiB minimum, 1 MiB target, and 4 MiB maximum.
- Chunk boundaries follow byte content to reduce shifted-boundary churn. Compression of the source file can defeat reuse.
- Transfers may batch small objects; physical storage may compress objects together. Logical object IDs do not depend on those physical containers.
- v1 does not require cross-object delta chains. This simplifies random reads but may use more storage than Git packs for many similar small-file revisions; benchmark it explicitly.

The snapshot is independently readable without replaying every prior change. History and blame indexes accelerate exploration but are rebuildable from authoritative records.

## Accepted history and exact builds

A channel entry names:

```text
main#1848
  previous: E1847
  accepted: C7K2@R3
  target: snapshot of E1847
  result: S92BC
  accepted-change-map: H_MAP
  policy: POLICY_12
  reviews: [APPROVAL_8]
  checks: [CI_91, CI_92]
```

A multi-change landing includes an ordered revision list and one final result snapshot. The whole list becomes accepted together. Intermediate integration snapshots may be retained for diagnostics but are not separate channel states unless explicitly accepted.

A channel's sequence-zero entry can reference a fork origin in another channel or an imported history head. A fork inherits that origin's snapshot and accepted-change map; later entries follow only their own channel's previous entry. The fork link preserves shared ancestry without requiring the channels to advance together. Import-history nodes preserve original multi-parent history, as detailed in [migration](06-delivery-and-validation.md).

Checks bind the candidate snapshot, ordered revisions, expected channel entry, policy version, and build-input definition. If `main` moves, the old candidate cannot be relabeled as tested against the new base.

Reviews approve revision bytes. A clean integration on a new base can retain that revision approval under the channel's policy, but requires checks on the new candidate. A manual conflict resolution creates a new revision and invalidates the old revision's approval.

History presents two views:

- **Change history:** how one proposal evolved, including abandoned revisions.
- **Channel history:** the exact sequence of accepted project states, suitable for bisect and releases.

## Serialization and portability

Proposed profile: deterministic CBOR encoding (a compact binary record format), typed/versioned objects, and SHA-256 object IDs prefixed with the algorithm name. **Canonical** means there is one agreed byte encoding for a record, so independent implementations compute the same ID. Field ordering, integer representation, forbidden duplicate keys, and exact hash framing MUST be frozen with reference examples before implementation interoperability is claimed.

The hash input includes object type, schema version, length, and canonical payload. Unsupported required schema versions cause an explicit error; readers do not reinterpret unknown bytes as an older object type.

Paths are byte-preserving, case-sensitive components; NUL, `/` inside a component, and traversal components `.`/`..` are forbidden. Platforms unable to materialize a path or a case collision must report it without rewriting the stored name. File modes cover ordinary/executable files and symlinks; Git links are preserved on import but are not native cross-project transactions.

## Invariants reviewers should challenge

- A snapshot ID always denotes the same bytes and file metadata.
- A shared channel entry never refers to incompletely durable required content.
- Duplicate network delivery cannot create duplicate acceptance.
- An unobserved concurrent revision is never discarded by a stale publication.
- Missing sparse data is never interpreted as a deletion.
- Approval of one revision never implicitly approves a revised one.
- A successful channel read pins one entry before any file lookup.
- Unchanged opaque subtrees are preserved by sparse edits.
- File history is reconstructible without a proprietary host's database.
- Changing physical storage placement never changes logical IDs.
