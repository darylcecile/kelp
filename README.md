# Kelp

Version control built around independent edits. Teammates working on different files can push without taking turns or rewriting each other's commits. A remote can distribute one project's files and history across storage nodes.

**Version 0.0.3:** an experimental, transaction-based release with partial checkouts. For another version, use the README at its release tag.

## Install

Download your platform's archive from [GitHub Releases](https://github.com/darylcecile/kelp/releases), extract `kelp` (`kelp.exe` on Windows), and place it on your `PATH`. Run `kelp --version` to check it. Releases include SHA-256 checksums.

Upgrading from 0.0.2? Full checkouts remain compatible, and partial clones work with 0.0.2 or newer remotes. Partial checkouts require a 0.0.3 or newer CLI. From 0.0.1, existing saved snapshots remain recoverable; explicitly commit the desired files to enter transaction synchronization and use a 0.0.2 or newer remote. Opening a 0.0.1 database upgrades its storage format, so retain a backup if you need to return to that binary.

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

Different-file commits combine without a pull-before-push requirement. If people commit different edits to the same file, both versions are retained and pull reports the conflict.

```sh
kelp pull --keep-local       # Or --keep-remote.
# Inspect/edit the result and run your tests.
kelp commit -m "Resolve the competing edits"
kelp push
```

The resolution records both alternatives as parents; it does not erase either original commit. Conflicts affecting the selected files must be resolved before a clean clone of those files can be made.

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

Kelp retains complete transaction metadata for the project, but downloads historical file contents only for selected paths. Cross-directory commits keep their original identity and meaning. This reduces file storage/transfer; it is not a path-permission boundary or a full backup. Copying history to another remote can require a full clone because excluded file contents are unavailable locally.

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

This restores the current folder and first saves a labelled backup of your work. The output gives the backup's view hash. Ignored files and Kelp metadata remain. In a partial checkout, only selected paths are restored, and saved-view hashes include that selection. Restore rejects views from a different selection. A commit hash alone is not a restore target because it may describe only an independent edit to one file.

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

On the documented 1,000-file/101-commit benchmark, maintained Kelp storage is about 324 KB versus Git's 330 KB. Results depend on the project; [the benchmark report](benchmarks/README.md#lossless-compaction-measurements) includes the read-time tradeoff.

## Current limits

- Regular, portable UTF-8-path files up to 16 MiB; no symlinks or large-file chunking yet.
- Conflicts are handled at file level, not automatically merged within a file.
- Partial selection is set when cloning; expanding an existing checkout is not yet supported. The complete transaction metadata is still downloaded.
- Replication, automated storage-node evacuation, Git import, and release-view pinning are future work.
- The distributed preview stores one copy of each object. Backups remain necessary for permanent node loss.

Kelp caches validated file state, compresses/batches object transfers, and can compact local storage into bounded packs. [Benchmark method and results](benchmarks/README.md) describe the measured space and latency, including comparisons where Git performs better.

## Help

Use `kelp --help` or `kelp <command> --help`. `--json` provides structured output and `-C PATH` selects a project directory.

- [Remote operation and protocol](apps/remote/README.md)
- [Report a problem](https://github.com/darylcecile/kelp/issues)
- [Development and source builds](CONTRIBUTING.md)
- [Architecture and product specification](PRD/README.md)
