# Kelp

Version control built around independent edits. Teammates working on different files can push without taking turns or rewriting each other's commits. A remote can distribute one project's files and history across storage nodes.

**Version 0.0.4:** transaction-based version control with large files, symlinks, text merging, partial checkouts, and replicated remote storage. For another version, use the README at its release tag.

## Install

Download your platform's archive from [GitHub Releases](https://github.com/darylcecile/kelp/releases), extract `kelp` (`kelp.exe` on Windows), and place it on your `PATH`. Run `kelp --version` to check it. Releases include SHA-256 checksums.

Use matching 0.0.4 CLI, gateway, and storage-node builds for the new features. Existing saved views and regular-file hashes remain valid. Workspaces upgrade to a format older CLIs cannot open. For existing distributed deployments, follow the [replica upgrade instructions](apps/remote/README.md#replication-and-node-evacuation) before enabling replicated operation.

## Save locally

```sh
kelp init my-project
cd my-project
# Create and edit files.
kelp diff
kelp commit -m "Add checkout"
```

No account or remote is required. New files are included automatically unless ignored by `.gitignore` or `.kelpignore`. Files already included remain known so edits and deletions can be saved.

Commits are deliberate. Editing files or inspecting history does not create background versions.

## Configure a remote once

Get a project URL and token from your remote's operator:

```sh
export KELP_TOKEN="<your access token>"
kelp remote set https://code.example.com/my-project
kelp push
```

In PowerShell, use `$env:KELP_TOKEN = "<your access token>"`.

**Push sends committed work only.** You can make several local commits and push when satisfied; unfinished edits stay on your machine. Interrupted transfers can be retried without changing commit IDs.

## Collaborate

```sh
kelp clone https://code.example.com/my-project
cd my-project
# Edit files.
kelp commit -m "Handle empty input"
kelp push
kelp pull
```

Different-file commits combine without a pull-before-push requirement. Pull also merges non-overlapping text edits using their common file ancestor. Original transactions remain intact; commit the merged result when ready. Overlapping edits, binary alternatives, and structural conflicts remain explicit:

```sh
kelp pull --keep-local       # Or --keep-remote.
# Inspect/edit the result and run your tests.
kelp commit -m "Resolve the competing edits"
kelp push
```

The resolution records both alternatives as parents; it does not erase either original commit. Clone can materialize a clean text merge. Overlapping conflicts affecting selected files must be resolved before cloning those files.

## Work on part of a project

```sh
kelp clone https://code.example.com/my-project --paths services/api
cd my-project
# Edit services/api.
kelp commit -m "Fix request validation"
kelp push
```

The selection is remembered for commit, pull, and restore. Files outside it are **not downloaded**, never treated as deletions. New files within the selection are included normally; files you create outside it stay local and are not committed.

Select several directories or individual files with commas or repeated options:

```sh
kelp clone https://code.example.com/my-project --paths services/api,libs/http --paths .gitignore
```

Paths are project-relative file/directory prefixes, not globs. `status` shows the active selection. Include root build files, ignore files, or other dependencies explicitly if your work needs them.

Kelp retrieves transactions relevant to your selection and their dependencies, plus historical file contents for selected paths. Unrelated history stays on the remote. Cross-directory transactions remain complete and keep their original identity. Selection controls downloading, not access permissions.

Expand your checkout whenever you need more of the project:

```sh
kelp pull --paths libs/http
kelp pull --paths .          # Download the rest of the project.
```

Existing drafts are preserved. If a newly selected path already contains different local work, pull asks you to choose a version with the usual conflict options. Expand fully before using a checkout as a complete project backup or copying all history to another remote.

## Import Git history

```sh
kelp import ../existing-git-project my-project
cd my-project
kelp push https://code.example.com/my-project
```

Import reads the selected Git revision and its ancestry, including merge results, file modes, symlinks, and reachable tags. Use `--ref BRANCH` to choose a revision. Author/committer information and original Git commit bytes are retained as provenance. Git must be installed for import. The source repository and its unfinished files are preserved; only committed Git bytes are imported. External Git LFS payloads and submodule projects remain external to that history.

## Pin a release view

```sh
kelp tag v1.0
kelp push
kelp restore v1.0
```

`tag NAME [VIEW_HASH]` gives a committed, complete saved view an immutable name. `kelp tag` lists names and view hashes. Tags travel with push, clone, and pull; they remain available after compaction. A tag cannot be moved onto a different view. Independently created conflicting names retain both views and require an exact hash to disambiguate.

## Inspect your work

```sh
kelp status           # Uncommitted files and commits ready to push
kelp diff             # Current file edits
kelp log              # Local saved views and recovery points
kelp show <view-hash> # Exact edits in a saved view
kelp log --commits    # Included edit transactions and their messages
kelp show <commit-id> # Inspect a shared commit; unique prefixes work
```

Both commits and saved views use hashes, displayed as short, unambiguous prefixes. A commit identifies a batch of edits; a saved view identifies the complete project state you can restore. `log` shows view hashes; `log --commits` shows commit hashes. Full hashes also work, and ambiguous prefixes produce an error.

## Restore in place

```sh
kelp restore <view-hash>
```

This restores the current folder and first saves a labelled backup of your work. The output gives the backup's view hash. Ignored files and Kelp metadata remain. Partial saved views restore their original selected paths, including after checkout expansion; newly included paths remain untouched. Expand first if a view covers paths you have not downloaded. A commit hash alone is not a restore target because it may describe only an independent edit to one file.

Restore changes working files. To share the restored result, commit it and push:

```sh
kelp commit -m "Restore the working checkout"
kelp push
```

## Reclaim storage

```sh
kelp gc
```

This losslessly packs stored objects and reclaims unused database pages. It keeps every commit, conflicting alternative, saved view, and recovery backup. Existing hashes continue to work. It does not commit unfinished files or push anything.

On the documented v0.0.2 1,000-file/101-commit benchmark, maintained Kelp storage was about 324 KB versus Git's 330 KB. Results depend on the project; [the benchmark report](benchmarks/README.md#lossless-compaction-measurements) includes the read-time tradeoff.

## Files and remote storage

Large files are streamed into reusable chunks automatically; the 16 MiB file ceiling is gone. Symlinks retain their targets rather than copying target contents. Native filenames, including non-UTF-8 names on Unix, are preserved losslessly. Checkout verifies that the destination filesystem can represent the requested names and symlinks before replacing files.

Distributed gateways default to two durable copies of objects, transaction journals, and release tags across storage nodes. With three nodes, one can be unavailable while reads and writes continue. The remote includes commands to repair replica placement and evacuate a retiring node. See the [remote guide](apps/remote/README.md#replication-and-node-evacuation) for deployment and recovery operations.

Kelp caches validated file state, compresses/batches object transfers, and can compact local storage into bounded packs. [Benchmark method and results](benchmarks/README.md) describe the measured space and latency, including comparisons where Git performs better.

## Help

Use `kelp --help` or `kelp <command> --help`. `--json` provides structured output and `-C PATH` selects a project directory.

- [Remote operation and protocol](apps/remote/README.md)
- [Report a problem](https://github.com/darylcecile/kelp/issues)
- [Development and source builds](CONTRIBUTING.md)
- [Architecture and product specification](PRD/README.md)
