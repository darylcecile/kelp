# Performance measurements

These scripts measure the current transaction-based prototype. They do not change existing repositories or publish anything remotely.

## Reproduce

Build optimized binaries following [CONTRIBUTING.md](../CONTRIBUTING.md#measure-performance), then run:

```sh
python3 benchmarks/compare.py --output benchmarks/results/local.json
python3 benchmarks/network.py --output benchmarks/results/network.json
```

Use `--kelp PATH` or `--bin-dir DIR` to compare another build. `--temp-dir DIR` places fixtures on a chosen disk. Run comparisons without concurrent builds or load-generating jobs.

## Local benchmark

Default fixture: 1,000 files of 4,096 bytes, followed by 100 one-file commits. Each status result contains 15 warm-cache samples; command startup is included. The incremental commit distribution covers all 100 edits.

Git is configured with untracked cache, preload index, filesystem cache, disabled signing, and automatic GC disabled during timing. Kelp commit is compared with Git add plus commit because both capture included working files. Both systems are measured before and after explicit GC, including maintenance time, subsequent status, and historical `show` latency. Kelp GC preserves all logical objects and recovery records. The test excludes global Git configuration for reproducibility.

This is a tuned **local baseline**, not every Git optimization: neither filesystem-monitor daemons nor sparse checkout are used. The working sets are complete and equivalent. No conclusion about WAN transfer, massive histories, or sharded write throughput follows from this benchmark.

Git uses its default persistence policy; Kelp uses SQLite WAL with full synchronous commits. Those durability policies are not equivalent, so the timings are user-visible operation measurements, not an isolated database-engine comparison.

The corpus is deliberately repetitive source-like text. Compression results do not generalize to already compressed images or encrypted files. Git stores history with delta packing; Kelp can group objects into bounded lossless packs and retains additional exact saved-view snapshots. Storage semantics differ and both sizes are reported.

## Network benchmark

Default fixture: 200 files of 4,096 bytes, one initial commit, then one single-file edit. A local HTTP proxy inserts 5 ms per request and counts requests and body bytes. It measures initial push, clone, incremental push/pull, and unchanged pull.

This isolates the effect of batching and compression between the CLI and one local gateway/store. Counts exclude HTTP headers and TLS overhead. A three-node deployment may incur additional internal storage requests; this fixture does not compare those with Git hosting.

## Results

Raw before/after JSON files in [results](results/) include fixture settings and local platform information. Before binaries were preserved before applying this optimization batch, and after binaries use the same Rust release profile and fixture scripts.

Local results on the recorded macOS/Apple Silicon machine:

| Measurement | Kelp before | Kelp after | Git (after run) |
| --- | ---: | ---: | ---: |
| Warm status, 101 commits, median | 197.591 ms | 16.493 ms | 13.137 ms |
| One-file commit, median | 163.447 ms | 30.386 ms | 37.888 ms (add + commit) |
| One-file commit, p95 | 202.669 ms | 37.616 ms | 48.708 ms |
| Initial commit, one observation | 43,109.922 ms | 27,281.738 ms | 497.337 ms |
| Metadata bytes before maintenance | 19,070,976 | 5,300,224 | 604,581 |
| Metadata bytes after Git GC | — | — | 330,075 |

The warm paths improved: about 12× faster status and 5.4× faster incremental commits than the previous Kelp build. Git still has lower status latency and much smaller storage. **Initial capture remains far slower than Git in this environment.** These measurements do not justify claiming general Git parity.

Network fixture results (5 ms added per request):

| Operation | Requests before → after | Elapsed before → after |
| --- | ---: | ---: |
| Initial push | 403 → 7 | 3,451 → 104 ms |
| Clone | 203 → 8 | 3,118 → 1,311 ms |
| One-file push | 6 → 6 | 55 → 55 ms |
| One-file pull | 4 → 6 | 261 → 171 ms |
| Unchanged pull | 2 → 2 | 46 → 25 ms |

Initial push request bodies fell from 848,695 to 68,115 bytes; clone response bodies fell from 848,880 to 50,741 bytes. These include metadata as well as file bytes. Small updates do not get the same batching benefit: the inventory stage can increase request count and metadata volume.

Interpret latency medians and tails together. A single initial-commit timing includes cold-start effects and is not a statistically stable result. Initial Kelp captures were much slower than incremental captures. A five-second macOS sample attributed 4,078 of 4,099 thread samples to the file-open syscall while reading new files. This identifies where time was spent, not the cause of the OS delay. Bounded parallel reads overlap that latency; initial timings remain disclosed rather than excluded from the results.

## Remaining costs

- Commands still enumerate files and stat metadata. Unix metadata-cache hits avoid rereading file contents; Windows conservatively rereads them until a reliable change counter is available.
- Current file-frontier maps and transaction membership IDs are serialized/loaded. Historical transaction bodies are not replayed for ordinary warm status/commit.
- New objects use per-object compression; GC packs related objects together with optional single-level, pack-local deltas. A cold historical lookup may decode a pack of up to 8 MiB, with two packs cached per reader thread. Cache records may reuse the local base snapshot as a dictionary.
- Small incremental network updates can use more requests than the previous single-object path because batches have an inventory stage.
- No production replication or linear scale-out throughput claim has been established.

## Lossless compaction measurements

`results/compacted.json` uses the same 1,000-file/101-commit fixture and includes `kelp gc` and `git gc`. Unlike the earlier per-object-compression measurements, Kelp now uses binary object/history indexes, compressed cache pages, and bounded packs with optional single-level copy/insert deltas. All logical objects and recovery events are retained.

Post-maintenance status and historical `show` timings include opening a new process for each sample, so they do not depend on a long-lived warm decoded-pack cache. The operating system's filesystem cache is warm. Packing has a separate maintenance cost and may trade some cold historical-read time for disk savings; those timings are reported, not assumed away.

Recorded result on the same macOS/Apple Silicon machine:

| Measurement | Kelp | Git |
| --- | ---: | ---: |
| Storage before maintenance | 4,534,272 bytes | 604,602 bytes |
| Storage after maintenance | **323,584 bytes** | **330,148 bytes** |
| Maintenance time | 144 ms | 176 ms |
| Incremental commit median, before maintenance | 30.3 ms | 37.5 ms (add + commit) |
| Status median, before maintenance | 18.0 ms | 12.8 ms |
| Status median, after maintenance | 20.6 ms | 11.6 ms |
| Historical show median, after maintenance | 12.8 ms | 10.7 ms |

Kelp is about 2% smaller than maintained Git **on this fixture**, while retaining its transaction model and all saved views. Relative to the previous Kelp measurement of 5,300,224 bytes, the maintained footprint is about 94% smaller. This is not a universal storage-size guarantee: file distributions, retained history, and cache state matter. Git remains faster for status and initial capture, and compaction adds a small read cost. The raw JSON records the initial-capture slowdown as well.
