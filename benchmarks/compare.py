#!/usr/bin/env python3
"""Reproducible local CLI latency/storage comparison; never modifies a real repo."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import statistics
import subprocess
import tempfile
import time


def run(args, cwd, env):
    start = time.perf_counter()
    subprocess.run(args, cwd=cwd, env=env, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    return (time.perf_counter() - start) * 1000


def summarize(samples):
    ordered = sorted(samples)
    return {"median_ms": round(statistics.median(samples), 3), "p95_ms": round(ordered[min(len(ordered) - 1, int(len(ordered) * .95))], 3), "samples": len(samples)}


def measure(tool, binary, parent, options):
    root = parent / tool
    root.mkdir()
    env = dict(os.environ, GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=os.devnull,
               GIT_AUTHOR_NAME="Benchmark", GIT_AUTHOR_EMAIL="benchmark@example.invalid",
               GIT_COMMITTER_NAME="Benchmark", GIT_COMMITTER_EMAIL="benchmark@example.invalid")
    for index in range(options.files):
        directory = root / f"src/{index // 100:04}"
        directory.mkdir(parents=True, exist_ok=True)
        seed = f"file {index:06}: reference data and code\n"
        (directory / f"file-{index:06}.txt").write_text((seed * (options.bytes // len(seed) + 1))[:options.bytes])
    if tool == "git":
        run(["git", "init", "-q"], root, env)
        for key, value in [("core.untrackedCache", "true"), ("core.preloadIndex", "true"), ("core.fscache", "true"), ("gc.auto", "0"), ("commit.gpgsign", "false")]:
            run(["git", "config", key, value], root, env)
        status = ["git", "status", "--porcelain"]
        def commit(message):
            return run(["git", "add", "."], root, env) + run(["git", "commit", "-qm", message], root, env)
    else:
        run([binary, "init", "--project", "benchmark"], root, env)
        status = [binary, "status", "--json"]
        def commit(message):
            return run([binary, "commit", "-m", message, "--json"], root, env)
    initial = commit("Initial fixture")
    def sample_status():
        run(status, root, env)
        return summarize([run(status, root, env) for _ in range(options.samples)])
    short_history = sample_status()
    commit_times = []
    for iteration in range(options.history):
        index = iteration % options.files
        with (root / f"src/{index // 100:04}/file-{index:06}.txt").open("a") as file:
            file.write(f"edit {iteration}\n")
        commit_times.append(commit(f"Edit {iteration}"))
    long_history = sample_status()
    metadata = root / (".git" if tool == "git" else ".kelp")
    size = lambda: sum(path.stat().st_size for path in metadata.rglob("*") if path.is_file())
    raw = size()
    maintained = None
    maintenance_ms = None
    maintained_status = None
    history_read = None
    if tool == "git":
        maintenance_ms = run(["git", "gc", "--quiet"], root, env)
        maintained = size()
        maintained_status = sample_status()
        history_read = summarize([run(["git", "show", "HEAD~50" if options.history >= 50 else "HEAD"], root, env) for _ in range(options.samples)])
    else:
        maintenance_ms = run([binary, "gc", "--json"], root, env)
        maintained = size()
        maintained_status = sample_status()
        versions = json.loads(subprocess.check_output([binary, "log", "--json", "--limit", str(min(options.history + 1, 1000))], cwd=root, env=env))
        selected = versions[min(50, len(versions) - 1)]["view"]
        history_read = summarize([run([binary, "show", selected], root, env) for _ in range(options.samples)])
    return {"initial_commit_ms": round(initial, 3), "status_short_history": short_history,
            "status_long_history": long_history, "incremental_commit": summarize(commit_times),
             "metadata_bytes": raw, "after_maintenance_bytes": maintained,
            "maintenance_ms": round(maintenance_ms, 3), "status_after_maintenance": maintained_status,
            "historical_show_after_maintenance": history_read,
            "metadata_note": "Git retains versions; Kelp also retains complete local recovery snapshots. Not identical storage semantics."}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kelp", default="target/release/kelp")
    parser.add_argument("--files", type=int, default=1000)
    parser.add_argument("--bytes", type=int, default=4096)
    parser.add_argument("--history", type=int, default=100)
    parser.add_argument("--samples", type=int, default=15)
    parser.add_argument("--temp-dir", default=None)
    parser.add_argument("--output", type=Path, required=True)
    options = parser.parse_args()
    if min(options.files, options.bytes, options.history, options.samples) < 1:
        parser.error("fixture sizes must be positive")
    binary = str(Path(options.kelp).resolve())
    report = {"environment": {"platform": platform.platform(), "cpu_count": os.cpu_count(),
                              "git": subprocess.check_output(["git", "--version"], text=True).strip(),
                              "kelp": subprocess.check_output([binary, "--version"], text=True).strip(),
                              "kelp_sha256": hashlib.sha256(Path(binary).read_bytes()).hexdigest()},
              "fixture": {name: getattr(options, name) for name in ("files", "bytes", "history", "samples")},
              "scope": "Warm filesystem cache, local SSD, separate command processes. Git add+commit vs Kelp commit. No network, fsmonitor daemon, sparse checkout, or production scale-out claims."}
    with tempfile.TemporaryDirectory(prefix="kelp-benchmark-", dir=options.temp_dir) as folder:
        for tool in ("git", "kelp"):
            report[tool] = measure(tool, binary, Path(folder), options)
    options.output.parent.mkdir(parents=True, exist_ok=True)
    options.output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
