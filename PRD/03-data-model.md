# 3. Atomic edit transactions

[Overview](README.md) · [Distribution](04-distribution.md) · [Protocol](05-protocol.md)

## A commit records writes, not a project position

The primary record is a transaction containing a message, a nonce, and a map of file edits. Each edit names its preceding file versions and its new value. A value can be a file object or a deletion.

```json
{
  "format": 1,
  "nonce": "client-generated-unique-value",
  "message": "Update the API and its caller",
  "edits": {
    "lib/api.rs": {
      "parents": ["T_API_PREVIOUS"],
      "value": {"blob": "B_API_NEW", "size": 123, "executable": false}
    },
    "app/caller.rs": {
      "parents": ["T_CALLER_PREVIOUS"],
      "value": {"blob": "B_CALLER_NEW", "size": 456, "executable": false}
    }
  }
}
```

IDs above are abbreviated placeholders. Real IDs are typed SHA-256 hashes. The nonce distinguishes independently authored transactions with identical messages and bytes. Transactions are immutable; amending the bytes creates a different transaction.

There is no root-tree field and no project-wide predecessor. An unchanged file creates no edit and no dependency. A new file has an empty parent set. A deletion has `value: null` and retains the causal history of the removed file.

## Dependencies are explicit and narrow

If a transaction consumes T1's version of `api.rs`, it depends on T1. T1's entire atomic batch must therefore be available, including edits it made to other paths.

Validation requires:

1. Every parent ID identifies a valid, complete transaction.
2. That parent actually wrote the path claiming it as a predecessor.
3. All references form an acyclic dependency graph.
4. Referenced file contents exist and match their declared hashes and lengths.
5. Paths cannot escape the workspace or overwrite its metadata.

These are file-history dependencies. They do not automatically discover semantic dependencies such as a function call in another file. Builds and tests remain necessary.

## Derive a view without choosing a total order

For any dependency-closed transaction set:

1. Add each transaction's output as a candidate value for each path it writes.
2. Remove each candidate explicitly consumed by another included edit.
3. The remaining candidates are that file's current alternatives.

This two-pass calculation does not depend on arrival order. Duplicate delivery has no effect because transactions are identified by hash.

```text
T0: file = base
       /       \
T1: file=A   T2: file=B

Candidates after {T0,T1,T2}: A and B

T3: parents={T1,T2}, file=resolved
Candidates after including T3: resolved
```

Equal concurrent values can be displayed as one value while retaining both causal IDs. An edit after that point consumes both parents. Delete-versus-edit is a conflict between a deletion value and file bytes, not an instruction to silently prefer deletion.

File/directory collisions are also conflicts: independently adding `a` and `a/b` cannot yield an ordinary directory tree until resolved.

## Atomic multi-file changes

A transaction is one immutable record. Discovery either includes that record, with all its edits and dependencies, or does not include it. File objects uploaded before publication are not visible project changes by themselves.

If T1 changed both the API and its caller, a reader cannot include just the API half of T1 in the logical view. Future sparse workspaces may materialize fewer files, but must retain the transaction's full logical meaning and completeness information.

This guarantee concerns versioned project state. Replacing multiple files in an ordinary working directory is not an atomic filesystem operation; local recovery snapshots protect interrupted updates.

## Snapshot and view identity

A **snapshot** identifies exact materialized files and modes. A **view** identifies a sorted set of transaction roots, whose dependency closure defines all included work.

```text
view roots {T1,T2} → dependency closure {T0,T1,T2} → file alternatives
                                                       │
                                             conflict-free snapshot
```

Two views may produce identical bytes while recording different intent or provenance. A conflicted view still has an exact identity, but cannot be treated as a clean build input.

There is no universally meaningful “the commit after T1” when T1 and T2 are independent. A dependency-sorted log is a display order, not an acceptance order. Releases can later pin a particular conflict-free view without putting every draft commit through a global sequencer.

## Local saves and unfinished files

The local workspace retains:

- Included transaction IDs and verified content.
- An outbox of explicitly committed transactions not yet acknowledged by the remote.
- A rendered base snapshot for detecting unfinished edits.
- Saved local views and labelled recovery snapshots with content hashes.
- Per-storage-node synchronization cursors and a layout fingerprint.

`commit` captures current files, computes edits against the known transaction view, stores the transaction and local snapshot durably, and adds its ID to the outbox. It creates dependencies only for paths it writes.

`push` transfers the outbox and any missing dependencies. It never scans current files to create another commit. Losing a response leaves the same immutable ID queued for retry.

`pull` combines received transactions with locally committed work. Unfinished file edits are separately combined against the rendered base. Conflicts stop replacement unless the user explicitly chooses a side. A side choice is a local draft resolution; a subsequent commit consumes the competing parents.

## Inspect and restore

`log` lists local saved-view hashes and recovery points; `show VIEW_HASH` compares a saved snapshot with its preceding local history entry. `log --commits` lists the included edit transactions; `show COMMIT_HASH` shows their file edits against the parent values. A resolution can have more than one relevant parent comparison.

`restore VIEW_HASH` restores the selected saved view in place and records a labelled backup first. It does not remove transactions or silently change the outbox. Committing restored files records new edits against the currently known file versions; pushing those edits does not rewind anyone else's history.

Cloning records a local view of the downloaded transaction set. Its transaction history is retained independently. It does not invent a global historical snapshot for each concurrent commit.

A saved-view object contains `format`, `snapshot`, and a sorted `roots` set. Its typed hash identifies exact files plus causal context. This differs from the transaction-set view ID, which can identify a conflicted graph or a graph whose unfinished working edits have not been committed. Local timestamps/messages describe history events without changing the saved-view object's hash. Older snapshot-only records migrate with empty roots because their causal set was not recorded.

### Selected-path views

Full saved views retain format 1 and their existing byte encoding/hashes. A partial saved view uses format 2 and additionally records a canonical `paths` list. Its snapshot contains only those paths, while its roots preserve the complete known transaction context. It does not claim that excluded file contents are present locally or that excluded conflicts are resolved.

Workspace metadata uses version 3 to fence extended file/path semantics from older clients. An empty selection means the complete project; otherwise scans and commits are restricted to selected prefixes. The retained frontier covers selected transactions and their full dependency closure. Indexed sync can omit unrelated transactions entirely. Only selected paths are compared with working files when constructing a new transaction.

This keeps atomic cross-directory transactions intact: their metadata is not rewritten into smaller transactions. The client may lack excluded blobs, and it must never substitute empty bytes for them. New partial commits push normally to a remote that already has their dependencies. A clone to another remote must obtain any missing outside-path dependency contents before those old transactions can be published there.

## Derived indexes and physical compression

The local projection cache stores current file alternatives and transaction roots. Status and commit load that cached frontier instead of rehashing and replaying transaction bodies. New commits and downloaded additions update it incrementally in dependency order. A membership fingerprint and payload checksum reject stale/corrupt cache records; rebuilding uses authoritative transactions.

Unix file-content reuse checks device, inode, length, mode, mtime, and ctime. A matching entry with timestamps older than its capture can reuse the blob ID. Replacements, same-size writes, mode changes, and racy timestamps force a reread. Platforms without a reliable change timestamp currently reread file content. These caches save no user-visible history.

Cold reads run in bounded groups of up to sixteen files with a 32 MiB expected-byte budget. A file that changes size during reading is rejected for retry. SQLite object writes remain on the calling thread inside the capture transaction.

Objects of at least 512 bytes are eligible for Zstandard level-1 compression when it saves space. SQLite records the codec and uncompressed length; hashes always cover logical bytes, and reads verify the hash after decoding. Uncompressed older rows remain readable. This is per-object compression, not Git-style inter-version delta packing.

`gc` adds a second, physical compression layer: related objects are grouped into self-contained Zstandard level-9 packs. Each object may use copy/insert instructions against one full base inside that pack. There are no recursive delta chains. The smaller of plain grouped compression and delta-grouped compression is retained. A binary object index records each object's offset, stored length, encoding, and original byte length. Retrieval reconstructs the exact canonical bytes and checks their original hash; no logical parent/dependency is introduced by packing.

Packs are limited to 256 objects and 8 MiB decoded, including any full base and delta instructions. A compaction candidate can contain up to 16 MiB of original logical bytes; it is split if neither representation fits the decoded pack budget. Individual objects larger than 8 MiB remain loose. Packs never cross a namespace, object kind, or storage database. A two-pack, per-thread decoded cache is bounded at 16 MiB. Compaction validates every input and round-trips packed bytes before atomically changing locations. A failed compaction rolls back, and free-page reclamation runs only after the location transaction commits.

Namespace/kind strings are interned and object/history hashes stored as 32-byte keys; the object index uses `WITHOUT ROWID`, with large payloads in separate tables. Workspace/projection metadata and sixteen hash-bucketed file-cache pages are compressed without dropping their contents. Compaction can dictionary-code rebuildable cache records against the local base snapshot, avoiding repeated file hashes while retaining fingerprint checks. That base is an immutable local object preserved by GC. Packing can alter random-read costs, so post-compaction status and historical reads are benchmarked alongside bytes.

No reachability-based deletion or retention expiry is performed by `gc`. Every logical object and recovery event remains available. Physical rows replaced by packs and unused database pages can be reclaimed without changing those guarantees.

Remaining costs include directory/stat traversal, current-file-map serialization, and loading membership IDs. The cache removes full-history **body replay** from common local operations; it does not make every operation constant-time.

## Encoding and compatibility

The transaction protocol is `kelp/1`. Transaction objects have `format: 1`, deterministic JSON field ordering, sorted paths, and sorted parent sets. File content retains the existing `kelp/0` typed-hash envelope so existing cached blob IDs remain valid. Object-format version and network-protocol version are distinct.

Regular files at most 16 MiB retain their original encoding. Larger files are streamed into 4 MiB chunks and bounded manifest trees (at most 1,024 children per manifest). Manifests and chunks use ordinary immutable blob storage, partitioning, and transfer. Child sizes sum to the parent size and strictly shrink; readers verify the complete tree. Changing one chunk reuses unaffected chunks. Symlink entries store target bytes rather than dereferenced contents.

Transaction format 2 permits extended file entries and an optional provenance entry. Git import uses provenance to retain exact original Git commit bytes, including signature headers, through synchronization. Existing format-1 transaction hashes remain unchanged. Metadata per transaction remains bounded at 2 MiB.

UTF-8 paths keep their existing keys. Non-UTF-8 components use a NUL-prefixed hexadecimal encoding, disjoint from literal filesystem names. Validation rejects traversal, metadata directories, embedded NUL bytes, and noncanonical aliases. Filesystem conversion is explicit; a checkout rejects names its host cannot represent before replacing files.

Release pins contain an immutable name, complete snapshot, and causal roots. Publication verifies that the roots derive exactly that committed snapshot. Equal names with different views remain distinct pins; lookup requires an exact view hash when ambiguous. Pins do not impose a global project head.

Local schema migration preserves previous saved snapshots and archives old workspace metadata. Existing snapshot-only histories require an explicit new commit to enter transaction synchronization. Standalone remotes retain v0 endpoints for older clients; the transaction API does not reinterpret v0 revision history as transaction history.

## Invariants

- A transaction ID is independent of unrelated work and remote arrival order.
- Independent edits commute; overlapping alternatives are retained.
- Every included transaction is complete, with its dependency closure.
- Resolutions explicitly consume the alternatives they resolve.
- No timestamp or storage-node arrival order chooses a file's winner.
- A view ID is reproducible across clients that know the same root set.
- Only explicit commits enter the push outbox.
- Repeated publication of the same transaction has one logical effect.
