# 2. Product requirements and workflows

[Overview](README.md) · [Transaction model](03-data-model.md)

## The ordinary workflow

```sh
kelp init shop
cd shop
kelp remote set https://code.example.com/shop  # Optional, configured once.
# Edit files.
kelp commit -m "Add checkout"
# Edit and commit more when ready.
kelp push
```

Local-only projects omit the remote and push steps. A commit is deliberate; push shares committed work and leaves unfinished files alone. New non-ignored files are included without a separate registration/staging step.

| Command | Meaning |
| --- | --- |
| `init [DIR]` | Start a local project |
| `clone URL [DIR] [--paths PATHS]` | Download transaction metadata and create a full or selected-path view |
| `status` | Show uncommitted files, queued commits, and unresolved alternatives |
| `diff` | Preview working-file edits against the current committed/pulled base |
| `commit -m MESSAGE` | Save an atomic edit transaction and a local recovery view |
| `push [URL]` | Transfer committed transactions; optionally remember the first URL |
| `pull` | Retrieve other work and combine independent edits |
| `log` | List local saved views and recovery points |
| `log --commits` | Inspect the included edit transactions |
| `show HASH` | Show exact edits in a saved view or transaction |
| `restore VIEW_HASH` | Restore a complete saved view in the current folder |
| `gc` | Compact physical storage without deleting logical history |
| `remote [set URL / remove]` | Inspect/change the optional destination |

## Save and inspect locally

```sh
kelp diff
kelp commit -m "Handle an empty cart"
kelp log
kelp show <view-hash>
```

A commit captures included files once, writes their bytes and edit transaction locally, and reports both its commit hash and saved-view hash. Its message and changed files are visible in history. Text files have line diffs; binary and executable-mode changes are explicit.

Only explicit commits enter the push outbox. Editing files, viewing status, and inspecting history create no commits. Pull and restore may save clearly labelled recovery snapshots before replacing files; those snapshots are not automatically shared.

Ignore rules exclude new files. Already included files remain known so modifications and deletions can be recorded. Project metadata directories are always excluded.

## Configure once, push when ready

```sh
kelp remote set https://code.example.com/shop
kelp commit -m "Add checkout"
kelp commit -m "Handle empty carts"
kelp push
```

Push sends the exact committed transactions and their required file/dependency objects. Edits made after the last commit are not included. A failed transfer leaves unacknowledged commits queued with their original IDs.

The prototype reads a remote access token from `KELP_TOKEN`. Configuring or removing a remote does not itself send data. Commit, inspect, and restore work without any credentials.

## Work independently without taking turns

Alice and Bob clone the same project. Alice edits `api.rs`; Bob edits `ui.rs`. They each commit and push without pulling the other's work first.

Both pushes succeed. Pulling later includes both transactions, with their original IDs and messages. There is no global branch-tip rejection or required merge commit for those independent edits.

This is a file-history guarantee, not an assurance that independently edited modules still behave correctly together. Application tests remain necessary.

## Resolve overlapping work

If both contributors edit `api.rs` from the same prior file version, both committed versions remain stored. A pull reports the competing values and leaves the working files unchanged until the user chooses how to proceed.

```sh
kelp pull --keep-local       # Or --keep-remote.
# Inspect/edit the chosen files and run tests.
kelp commit -m "Resolve the API edits"
kelp push
```

The resolution commit names both file parents, so replicas agree that both alternatives were considered. It is not a timestamp-based overwrite. `show COMMIT_ID` can still inspect either original edit afterward.

The prototype's choices are whole-file choices. Several different incoming alternatives may require keeping the local file, editing it manually, and committing the result. Richer per-file/text resolution is future work.

A clean clone of a conflicted shared view currently stops with an explanation. An existing contributor must resolve and push before a clean clone is available. This is an explicit consequence of accepting conflicting contributions without a globally serialized clean-head gate.

## Partial checkouts

```sh
kelp clone https://code.example.com/shop --paths services/api,libs/money
cd shop
kelp commit -m "Update selected services"
kelp push
```

The clone persists a canonical set of project-relative files/directory prefixes. Prefix matching respects path boundaries: `services/api` does not select `services/api-old`. Commas and repeated `--paths` arguments combine selections; redundant child prefixes are collapsed. `--paths .` means a full clone.

Complete transaction bodies and dependency metadata are retained. Only selected paths' historical file blobs are fetched. A commit computes edits from the selected file frontier; absent outside files do not become deletions. Pull retains the selection, including when a new remote commit changes selected and unselected paths atomically. Source conflicts entirely outside the selection do not block work; structural collisions affecting selected paths are still reported.

Saved partial views include their path scope in their hash. Restore requires a matching scope and leaves outside files untouched. Inspection of a cross-directory commit marks excluded file bodies as not downloaded rather than omitting their metadata. A partial checkout can push its new commits to the original remote, which already holds their complete dependencies. Exporting those dependencies elsewhere can require uncached outside blobs and fails explicitly when they are missing.

The selection is fixed for this release. Include root ignore/build configuration explicitly when needed. This is selective content replication, not metadata secrecy or a full project backup.

## Understand saved views versus shared commits

`log` is a local recovery timeline: these are exact project states this folder saved. It displays saved-view hash prefixes. The full hash binds the snapshot and its causal root set, independent of any local database row number.

`log --commits` lists the shared edit transactions in a dependency-respecting display order. Independent transactions have no intrinsic order relative to each other. Their hashes are stable references; `show` accepts a full or unambiguous prefix.

Cloning creates one local saved view of the downloaded set and retains the complete transaction history separately. It does not invent a project-wide historical position for every independent commit.

## Restore in place

```console
$ kelp restore a83b19ec702f
Restored view a83b19ec702f in this folder.
Previous work is saved as view c928a03640df.
```

The operation saves current included files as a labelled backup, restores the requested file contents/modes, removes included files absent from the target, and preserves ignored files and metadata.

Restore changes working files, not committed transactions or remote history. To share a rollback:

```sh
kelp restore a83b19ec702f
kelp commit -m "Restore the working checkout"
kelp push
```

The new commit records reverse edits against known file versions. Concurrent work the user has not observed cannot be silently erased by moving a global history pointer.

Hash prefixes must contain at least four lowercase hexadecimal characters and resolve uniquely. Output starts at twelve characters and extends when needed. `view:` and `commit:` prefixes can restrict lookup to an object kind. Restore rejects commit hashes: independent edits do not by themselves specify a complete project state.

## Compact without losing work

`kelp gc` packs related stored objects, compacts metadata, and reclaims free database pages. It preserves every commit, saved-view hash, conflict alternative, queued commit, and recovery point. It does not create commits or upload unfinished files. Compression may use one full pack-local base; that is not a causal dependency and never requires another storage node to decode an object.

## Acceptance criteria

| Requirement | Verification |
| --- | --- |
| Local work has no remote prerequisite | Init, commit, diff/show/log, and restore work offline |
| Commit and push have separate responsibilities | Unfinished edits remain absent from a fresh clone after push |
| Independent edits remain independent | Both clients push without pull or rewritten IDs |
| Conflicts preserve alternatives | Both values survive and resolution consumes both parents |
| Atomic multi-file changes | A missing file object prevents the whole batch from appearing |
| In-place recovery | Restored files match the saved view; the prior files can be recovered |
| One project can span nodes | Objects and journal entries physically occupy several databases |
| Gateways are interchangeable | A fresh gateway reads existing work without importing a project database |
| Capacity can grow | A fourth member accepts new work while old IDs remain readable |
| Familiar basic interface | No manually created transaction IDs, background services, or staging state |

Branches, review workflows, selected-path replication, native bundles, and Git import remain advanced work. They must build on this transaction model rather than force everyday edits through a single project-wide acceptance head.
