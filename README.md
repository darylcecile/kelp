# Kelp

Local-first, change-oriented version control. Edit files, then publish; further publications update the same change.

Kelp automatically keeps local checkpoints so you can recover earlier work. Use it entirely on your own machine, or connect a remote to share changes with others. This is an early release; see [current limits](#current-limits) below.

## Install

1. Download the archive for your system from [GitHub Releases](https://github.com/darylcecile/kelp/releases): Linux x86-64, macOS Intel/Apple Silicon, or Windows x86-64.
2. Extract `kelp` (or `kelp.exe` on Windows).
3. Move it into a directory on your `PATH`.
4. Run `kelp --version` to check the installation.

Each release includes `SHA256SUMS` for verifying downloads.

## Start entirely locally

```sh
mkdir my-project
cd my-project
kelp init

# Create files in your editor, then include them in version control:
kelp track src
kelp status
kelp log

# Optional named checkpoint:
kelp checkpoint -m "Before refactoring"

# Recover a checkpoint into a new directory:
kelp restore 1 --to ../recovered-project
```

`init` needs no network, token, or remote. It tracks existing regular files, respecting `.gitignore` and `.kelpignore`. Add later files with `kelp track PATH`. Tracked deletions remain part of subsequent snapshots.

Automatic checkpoints start with `init` and `open`. They capture tracked bytes saved by your editor, not unsaved editor buffers. `kelp status` reports the watcher, and `.kelp/watch.log` contains capture errors. Use `kelp watch --stop` to stop the process, `kelp watch` to restart it, or `--no-watch` on `init`/`open` in automation. All checkpoints are retained in v0.

## Share a change

Get a remote URL and access token from your remote's operator. In your project's shell:

```sh
export KELP_TOKEN="<your access token>"
kelp remote set https://code.example.com
kelp publish -m "Fix checkout timeout"

# Edit tracked files after feedback, then:
kelp publish
kelp changes
```

The first publish creates the project on the remote and the change identity. Later publications keep that identity. Unchanged work reports “already published.” Failed publications retain their exact revision and request ID locally for retry. The access token is read from the environment and is not written into workspace metadata.

In PowerShell, set the token with `$env:KELP_TOKEN = "<your access token>"` instead of `export`.

## Open shared work

Open a published change into a new directory using the change ID printed by publish:

```sh
kelp open https://code.example.com ../review-copy \
  --project my-project --change <change-id>
```

An open change is editable and can be republished from either workspace. Concurrent revisions are both retained. When a change has multiple heads, `open` requires an explicit `--revision <object-id>`.

`kelp change new "Next task"` starts separate work based on the current files. `kelp remote remove` returns the workspace to local-only operation without deleting checkpoints. `--json` provides machine-readable command output, and `-C PATH` selects another working directory.

## Current limits

- Regular files up to 16 MiB, with UTF-8 paths. Symlinks are not supported yet.
- Each checkpoint's file listing is limited to 2 MiB. Large-project optimizations are still in development.
- `open` retrieves the selected change's base and result, not a full project mirror.
- Merging, landing into shared channels, review approvals, Git import, and selecting individual hunks are not available yet.

On Unix, executable bits are retained. Windows uses ordinary file permissions while preserving imported executable metadata.

## More help

Run `kelp --help` or `kelp <command> --help` for command options.

- [Report a problem](https://github.com/darylcecile/kelp/issues)
- [Host a remote](apps/remote/README.md)
- [Development and source builds](CONTRIBUTING.md)
- [Product roadmap](PRD/README.md)
