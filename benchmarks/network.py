#!/usr/bin/env python3
"""Measure Kelp HTTP request counts/bytes with controlled per-request latency."""
import argparse
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import http.client
import json
import os
from pathlib import Path
import secrets
import socket
import subprocess
import tempfile
import threading
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, default=Path("target/release"))
    parser.add_argument("--files", type=int, default=200)
    parser.add_argument("--latency-ms", type=float, default=5)
    parser.add_argument("--temp-dir", default=None)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    binary = args.bin_dir.resolve()
    token = secrets.token_hex(24)
    env = dict(os.environ, KELP_TOKEN=token)
    metrics = {"requests": 0, "request_body_bytes": 0, "response_body_bytes": 0}
    lock = threading.Lock()
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        port = probe.getsockname()[1]

    class Proxy(BaseHTTPRequestHandler):
        def forward(self):
            body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
            time.sleep(args.latency_ms / 1000)
            upstream = http.client.HTTPConnection("127.0.0.1", port, timeout=30)
            upstream.request(self.command, self.path, body=body, headers={"Authorization": self.headers.get("Authorization", ""), "Content-Type": self.headers.get("Content-Type", "application/octet-stream")})
            response = upstream.getresponse()
            payload = response.read()
            with lock:
                metrics["requests"] += 1
                metrics["request_body_bytes"] += len(body)
                metrics["response_body_bytes"] += len(payload)
            self.send_response(response.status)
            self.send_header("Content-Type", response.getheader("Content-Type", "application/octet-stream"))
            self.send_header("Content-Length", response.getheader("Content-Length", str(len(payload))))
            self.end_headers()
            if self.command != "HEAD": self.wfile.write(payload)
            upstream.close()
        do_GET = do_HEAD = do_POST = do_PUT = forward
        def log_message(self, *_): pass

    with tempfile.TemporaryDirectory(prefix="kelp-network-", dir=args.temp_dir) as folder:
        root = Path(folder)
        with (root / "remote.log").open("w") as log:
            process = subprocess.Popen([str(binary / "kelp-remote"), "--listen", f"127.0.0.1:{port}", "--data-dir", str(root / "data")], env=env, stdout=log, stderr=log)
            proxy = ThreadingHTTPServer(("127.0.0.1", 0), Proxy)
            thread = threading.Thread(target=proxy.serve_forever, daemon=True)
            try:
                deadline = time.monotonic() + 15
                while True:
                    try:
                        connection = http.client.HTTPConnection("127.0.0.1", port, timeout=1)
                        connection.request("GET", "/healthz")
                        assert connection.getresponse().status == 200
                        connection.close()
                        break
                    except OSError:
                        if process.poll() is not None or time.monotonic() >= deadline: raise
                        time.sleep(.05)
                thread.start()
                url = f"http://127.0.0.1:{proxy.server_port}/demo"
                def cli(directory, *commands):
                    result = subprocess.run([str(binary / "kelp"), "-C", str(directory), *commands], env=env, capture_output=True, text=True, timeout=120)
                    if result.returncode: raise RuntimeError(result.stderr)
                def measure(directory, *commands):
                    with lock:
                        for key in metrics: metrics[key] = 0
                    started = time.perf_counter()
                    cli(directory, *commands)
                    return {"elapsed_ms": round((time.perf_counter() - started) * 1000, 3), **metrics}
                cli(root, "init", "source", "--project", "demo")
                source = root / "source"
                for index in range(args.files):
                    (source / f"file-{index:05}.txt").write_text((f"file {index} repeated line\n" * 300)[:4096])
                cli(source, "commit", "-m", "Initial files")
                report = {"binaries": {name: hashlib.sha256((binary / name).read_bytes()).hexdigest() for name in ("kelp", "kelp-remote")},
                          "fixture": {"files": args.files, "bytes_per_file": 4096, "latency_ms_per_request": args.latency_ms},
                          "scope": "One localhost gateway/store. Request bodies exclude HTTP/TLS headers. This measures batching, not scale-out or Git parity."}
                report["initial_push"] = measure(source, "push", url)
                report["clone"] = measure(root, "clone", url, "copy")
                (source / "file-00000.txt").write_text("one changed file\n")
                cli(source, "commit", "-m", "Update one file")
                report["incremental_push"] = measure(source, "push")
                report["incremental_pull"] = measure(root / "copy", "pull")
                report["unchanged_pull"] = measure(root / "copy", "pull")
                assert (root / "copy/file-00000.txt").read_text() == "one changed file\n"
                args.output.parent.mkdir(parents=True, exist_ok=True)
                args.output.write_text(json.dumps(report, indent=2) + "\n")
                print(json.dumps(report, indent=2))
            finally:
                if thread.is_alive(): proxy.shutdown()
                proxy.server_close()
                process.terminate()
                process.wait(timeout=15)


if __name__ == "__main__":
    main()
