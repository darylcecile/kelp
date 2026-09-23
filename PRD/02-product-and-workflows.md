# 2. Product requirements and everyday workflows

[Overview](README.md) · [Data model](03-data-model.md)

All commands and output below are proposed interfaces. IDs are shortened for readability; they are not executable demonstrations. The workflows are separate scenarios, not one continuous transcript.

## Who this is for

- **Individual developers:** save work, switch tasks, and recover mistakes without learning reference archaeology.
- **Reviewers and maintainers:** follow a change across revisions and land exactly what was validated.
- **Large-project teams:** work on a small part of a huge codebase without splitting its atomic history.
- **CI workers:** acquire exact inputs efficiently and reuse local or regional caches.
- **Self-hosters and independent communities:** move projects, including their reviews, between providers.

The initial adoption target is a source-code team with frequent change revisions or stacked reviews. Extreme-scale hosting is a later milestone, not a prerequisite for trying the client.

## Everyday workflow

Edit files, then publish. After feedback, edit and publish again. Kelp automatically checkpoints on-disk work locally while you work; you name a change when you first share it.

Work can start with `kelp init` in a local directory or with `kelp open` from a remote. A local-only workspace supports checkpoints, history, and recovery indefinitely; publishing is only needed when sharing with a configured remote.

```sh
kelp publish -m "Fix checkout timeout"  # First version of this work
# Edit files after feedback.
kelp publish                           # Updated version of the same work
```

## Concepts behind the workflow

| Concept | Plain meaning | Example |
| --- | --- | --- |
| Workspace | A directory where you work, with its own saved task state | `checkout-fix` |
| Change | A unit of work that keeps its identity | `C7K2: Fix checkout timeout` |
| Revision | One exact saved version of that change | `C7K2@r3` |
| Snapshot | An exact complete project state, even if only partly downloaded | `S91AF` |
| Channel | A named sequence of accepted snapshots | `main`, `release/2.x` |

“Publish” records and shares a proposal. “Land” adds an exact revision to a channel. Automatic checkpoints stay local; publishing does not advance `main`.

## Requirements

| ID | Required behavior | Acceptance example |
| --- | --- | --- |
| P1 | Checkpoint work automatically without a server | Disconnect, edit files, recover an automatic checkpoint, and mark a named checkpoint |
| P2 | Keep identity across revisions | Review URL and change ID survive a message edit and rebase |
| P3 | Support selective publication without hidden state | Publish selected hunks, or test an exact checkpoint before publishing it |
| P4 | Preserve explicit competing edits | Concurrent publication exposes both revisions; neither is overwritten |
| P5 | Represent conflicts as saved state | Suspend a conflicted change, do other work, and resume later |
| P6 | Publish atomic, exact integration snapshots | A reader sees either the old or new project state, never half a landing |
| P7 | Make data availability visible | Explain whether a history query is complete, uncached, or unavailable |
| P8 | Move one project's storage across nodes transparently | Repartition while reads and unrelated publishes continue correctly |
| P9 | Export useful history and collaboration data | Restore a full native mirror without the original provider |
| P10 | Integrate with existing Git users | Import history and expose accepted channels as Git branches |
| P11 | Create and revise proposals through one publish command | First publish creates an identity; subsequent publishes retain it |
| P12 | Support local-only work with no remote dependency | Initialize, checkpoint, inspect, and recover with networking disabled; attach a remote later without uploading |

## Local-only setup

```console
$ kelp init my-project
Created a local workspace. No remote configured.
Automatic local checkpoints started.

$ kelp checkpoint -m "Before refactoring"
Recorded a named local checkpoint.

$ kelp log
Recent local checkpoints, including automatic captures

$ kelp restore 1 --to ../recovered-project
Recovered checkpoint 1 into a new directory.
```

`init` tracks existing supported, non-ignored files; `kelp track PATH` includes new files created later. Local storage holds the captured bytes, so recovery does not depend on a promisor remote. All ordinary local operations work without credentials. A fresh directory can be initialized empty and populated later.

Sharing remains an explicit choice:

```console
$ kelp remote set https://code.example
Remote configured. No data uploaded.

$ kelp publish -m "Share the initial implementation"
Created the change and shared its first revision.

$ kelp remote remove
Remote detached. Local files and checkpoints retained.
```

Without a configured remote, `publish` explains how to attach one; it does not silently select a hosted service. Local-only users do not need to publish. A workspace originally opened sparsely from a remote retains its existing completeness limits after detachment; removing the URL does not materialize missing data.

## Workflow A: make and review a change

```console
$ kelp open https://code.example/acme/shop --paths services/checkout,libs/money
Opened shop at main#1842 (S91AF).
Downloaded the selected paths. Older content will be fetched when requested.
```

Edit files in your editor, then:

```console
$ kelp status
New work in workspace default
2 edited tracked files; latest local checkpoint O80
Base: main#1842; last checked 6 minutes ago
Not published

$ kelp publish -m "Fix checkout timeout"
Created C7K2 and published its first revision, r1, to acme/shop.
Durability: regional (survives one storage-node failure).
Review: https://code.example/acme/shop/changes/C7K2
```

After feedback, edit the files and publish again:

```console
$ kelp publish
Recorded and published C7K2@r2. Previous comments remain attached to r1.
Approval of r1 does not approve r2.

$ kelp review approve C7K2@r2
Approved the exact r2 revision.

$ kelp land C7K2@r2 --into main
Queued L83. Validating an integration candidate against main#1847.

$ kelp land status L83
Landed at main#1848 (S92BC). Validated candidate: S92BC.
```

Approval permissions and required checks belong to channel policy. A small local project can have neither. The acceptance transaction still identifies exact inputs and output.

`r2` is a local/host display alias for a full immutable revision ID. Scripts should use full IDs from `--json`; concurrent revisions do not have a universal numeric order.

### What publish includes

- With no selector, `publish` captures the current tracked-file edits for the workspace's active work, records an immutable revision locally, then uploads it. Later editor writes belong to a later revision.
- On first publication it creates the change identity locally. `-m` supplies its title; an interactive terminal opens a title editor if needed. Scripts must supply a title for unnamed work.
- Further publications update that change. An unchanged proposal reports “already published” rather than creating an empty revision. `-m` can update its description as part of a new revision.
- `publish --revision C7K2@r2` sends exactly that existing revision. `publish --checkpoint O91` records a revision from exactly that checkpoint. Neither includes subsequent workspace edits.
- If the network fails, the revision remains local. The next publish resolves the pending request first, using its original identity and revision, before publishing any newer edits.

### Start a separate piece of work

Use `kelp change new "Add coupon support" --onto main` when the current change is still open and you want to start an independent task. It checkpoints and suspends the current task, then starts a separate draft at the last-known `main` snapshot. `status` names the active task so later publishes have a clear destination. To return, use `kelp change switch C7K2`.

This explicit task boundary is optional for the first change. Once Kelp observes that the active change has landed, the landed revision's local result becomes the base for a fresh draft; later edits are preserved and the next publish creates a new change. Kelp never guesses that edits to an open change represent a different task.

## Workflow B: keep partial-change power

Suppose a file contains both a bug fix and unrelated cleanup.

```console
$ kelp publish --interactive -m "Fix checkout timeout"
Select edits for C7K2: [selected bug-fix hunks]
Published C7K2@r3. Unselected cleanup remains local.
```

Alternatively, to test selected work before sharing it, mark an exact local checkpoint:

```console
$ kelp checkpoint --interactive -m "Bug fix ready for testing"
Recorded selected bug-fix hunks as checkpoint O91. Working files unchanged.

$ kelp workspace add ../verify-checkout --at O91
Created an isolated workspace containing exactly O91's snapshot.
```

Run ordinary project tests in `../verify-checkout`, then publish that checkpoint:

```console
$ kelp publish --checkpoint O91 -m "Fix checkout timeout"
Published the exact O91 snapshot as a revision of C7K2.
```

Selection belongs to that one operation. A later plain `publish` captures all current tracked-file edits, including any remaining cleanup; it does not remember an earlier hunk selection. The original directory still contains that cleanup, so tests run there cannot be described as tests of the selected snapshot.

## Workflow C: switch tasks and recover

Assume an `urgent-fix` workspace already exists.

```console
$ kelp workspace switch urgent-fix
Checkpointed tracked edits for workspace default as O81.
Switched to urgent-fix.

$ kelp workspace switch default
Restored C7K2, including its unfinished edits.

$ kelp history operations
O83  switch to default
O82  switch to urgent-fix
O81  checkpoint before switch

$ kelp undo O82
Restored the prior workspace selection. Recovery record: O84.
```

Contract:

- Opening a workspace starts a local watcher that checkpoints tracked-file changes after a short idle interval. Checkpoints contain bytes saved to disk by the editor; they never trigger an upload.
- `status` reports the latest durable checkpoint and whether capture is active, pending, or unavailable. Recovery covers completed checkpoints, not every keystroke or edit between captures.
- `kelp checkpoint -m "Before retry refactor"` immediately records and pins a named local checkpoint. This is optional and works offline.
- Before a Kelp command overwrites tracked workspace bytes, it checkpoints them durably on the local disk.
- Newly created files become tracked through `kelp track <path>`; ignored files are excluded.
- A switch refuses to overwrite an untracked file and names the obstructing path.
- Automatic recovery records default to 30 days of retention; active task state and named checkpoints remain pinned until released.
- Unsaved editor buffers and disk writes not yet captured are outside the recovery guarantee. If automatic capture is unavailable, an explicit checkpoint still works when local storage is writable.
- Undo adds a recovery operation. It does not rewind a shared channel or recall already published bytes.

To back out accepted work, use `kelp revert C7K2 --from main`. This creates a new change, which may conflict with later edits and follows the normal review/landing workflow.

## Workflow D: stacked work

A second change uses an API introduced by the first.

```console
$ kelp change new "Use bounded retries" --after C7K2@r3
Started a separate draft requiring C7K2@r3.
```

Edit the new task, then:

```console
$ kelp publish
Created and published C8M4@r1. Requires C7K2@r3.

$ kelp stack show
C7K2@r3  Fix checkout timeout
└─ C8M4@r1  Use bounded retries
```

Revising C7K2 does not silently change the revision C8M4 depends on.

```console
$ kelp stack update --root C7K2@r4
Prepared updated descendants.
C8M4@r2 needs resolution in services/checkout/retry.ts.
Previous revisions remain available.
```

The update is a local recorded operation. Publishing a stack advertises its exact revision set together. Landing a stack applies dependency order and atomically accepts the entire selected set. A missing prerequisite produces an actionable dependency error, not an implicit fetch-and-land of unreviewed work.

## Workflow E: conflicts and concurrent revision

```console
$ kelp change update C7K2 --onto main
Saved C7K2@r5 with 1 unresolved conflict.
services/checkout/retry.ts:
  base: C7K2@r4 base snapshot
  proposed: C7K2@r4 by Maya
  target: main#1850

$ kelp conflicts show
F62  retry.ts  incompatible edits to timeout handling

$ kelp conflicts resolve F62 --editor
$ kelp publish
Recorded and published C7K2@r6; F62 resolved.
```

You can publish a conflicted revision for help. It cannot land on an accepted channel. Text markers are an editing aid; the stored conflict retains its input objects even after the markers disappear.

If Maya and Leo both edit from r6 offline, their local checkpoints retain both attempts. Publishing when reconnected creates divergent revisions:

```text
             ┌── r7-maya ──┐
r6 ──────────┤             ├── r8, reconciled explicitly
             └── r7-leo ───┘
```

`kelp change reconcile C7K2` creates a revision naming both predecessors. A timestamp never decides whose work wins. A stale publish may add a competing head; it cannot erase a head it did not observe.

## Workflow F: sparse and offline work

```console
$ kelp offline prepare --paths services/checkout,libs/money --history 30d
Pinned selected-path snapshots and history for the requested interval.
Also pinned the bases and inputs of your active changes.
Older history and other paths remain online-only.

$ kelp offline status
Ready for offline edits and diffs within the pinned scope.
Build dependencies outside that scope have not been assessed.
```

A time interval means accepted snapshots in that interval and the boundary snapshot needed to interpret its first change; imported history uses recorded timestamps and reports its selection. It is a data-selection rule, not a claim that clocks give a reliable causal order.

Offline capability includes automatic and named checkpoints, diffing cached revisions, resolving conflicts with cached inputs, and creating bundles. `kelp bundle create fix.kelp --checkpoint O91 -m "Fix checkout timeout"` prepares a proposal from a checkpoint without contacting a host. Offline work excludes obtaining uncached bytes, learning the latest shared state, or landing on a remote authority.

`kelp workspace expand --paths libs/http` adds files at the workspace's existing snapshot. Expansion must not accidentally mix files from a newer `main`.

## Workflow G: big files and reproducible CI

Large files use the same automatic-checkpoint and publish lifecycle as source. They have manifests of bounded chunks rather than a separate user-managed pointer system. Changing a few bytes can reuse unchanged chunks, but compressed or encrypted assets may still require uploading most of the file.

```console
$ kelp checkout --snapshot S92BC --paths services/checkout,libs/money --to build-src
Materialized exact snapshot S92BC for the selected paths.
```

A build input record includes the full snapshot ID, selected paths, and external dependency lock/toolchain IDs. The snapshot alone does not capture the operating system, network services, or package downloads.

For non-mergeable assets, v1 reports competing binary versions and requires an explicit selection or replacement. Enforced asset locking is deferred; promising offline exclusive locks would be misleading.

## Workflow H: releases and independent exchange

```console
$ kelp channel create release/2.x --from main#1848
Created release/2.x at S92BC, inheriting main#1848's accepted history.

$ kelp tag create v2.0 --at release/2.x#0
Created an immutable release label for the exact entry and snapshot.

$ kelp bundle create checkout-fix.kelp --change C7K2@r2
Created a proposal bundle with its required objects and provenance.

$ kelp bundle verify checkout-fix.kelp
Complete for the declared proposal scope.
```

A colleague can import the bundle without an account at the original host. Importing makes the proposal available locally; it does not advance their channel. Signing and trust rules are the same as for network exchange.

Release channels advance independently. Tags are immutable labels, not moving channel names. Native `log`, `diff`, `blame`, and `bisect` operate on exact revisions or channel entries, with missing-data requirements reported before offline execution.

## UX and automation rules

- `status` is local by default. `status --refresh` contacts the channel authority.
- Error output says what happened, what remains saved, and the next useful command.
- Every command supports structured `--json` output with stable error codes.
- Inspecting an old snapshot is read-only; editing from it creates a local draft automatically, named on first publish.
- Read-only inspection does not secretly publish or modify shared state.
- A workspace is single-writer at the file-materialization layer; concurrent agents use separate workspaces sharing a content cache.
- Renames recorded with `kelp move` retain file identity. Moves made in an editor may require confirmation; uncertain inference is not authoritative.

## Out of scope for the first release

Live multi-cursor editing, universal semantic merging, a complete issue tracker, a new build system, global permissionless discovery, and atomic updates across independently administered projects.

Portable revision comments and approvals are in scope because they define what a change means during collaboration. A full hosting platform is not required.
