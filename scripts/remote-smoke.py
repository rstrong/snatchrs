#!/usr/bin/env python3
"""Test a standalone binary using only Python's standard library and loopback HTTP."""
import argparse
import hashlib
import http.server
import json
from pathlib import Path
import subprocess
import tempfile
import threading


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--temp-root", type=Path)
    args = parser.parse_args()
    bodies = {"remote_smoke-1.0.whl": b"wheel fixture bytes", "remote_smoke-1.0.tar.gz": b"source fixture bytes"}
    requests = []

    class Handler(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            requests.append(self.path)
            if self.path == "/simple/":
                body = json.dumps({"meta": {"api-version": "1.0"}, "projects": [{"name": "remote-smoke"}]}).encode()
            elif self.path == "/simple/remote-smoke/":
                body = json.dumps({"name": "remote-smoke", "meta": {"api-version": "1.4"}, "versions": ["1.0"], "files": [
                    {"filename": name, "url": f"http://127.0.0.1:{self.server.server_port}/{name}",
                     "size": len(data), "hashes": {"sha256": hashlib.sha256(data).hexdigest()}}
                    for name, data in bodies.items()
                ]}).encode()
            else:
                body = bodies[self.path.lstrip("/")]
            self.send_response(200)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *_):
            pass

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        with tempfile.TemporaryDirectory(prefix="snatchrs-smoke-", dir=args.temp_root) as directory:
            root = Path(directory)
            common = ["--mirror", str(root), "--package", "remote-smoke", "--metadata-url", f"http://127.0.0.1:{server.server_port}/simple/", "--json"]

            def run(command, *extra):
                result = subprocess.run([str(args.binary.resolve()), command, *common, *extra], capture_output=True, text=True, timeout=30)
                if result.returncode:
                    raise RuntimeError(result.stderr)
                return json.loads(result.stdout)

            first = run("check")
            assert first["summary"]["missing"] == 2
            package = root / "packages/remote-smoke"
            (package / "remote_smoke-1.0.whl").write_bytes(b"partial")
            check = run("check")
            assert check["summary"]["size_mismatch"] == 1
            assert check["summary"]["missing"] == 1
            report = run("mirror", "--concurrency", "2")
            assert report["downloads"]["progress"]["downloaded"] == 2
            assert report["final_check"]["matching_size"] == 2
            for name, data in bodies.items():
                assert (package / name).read_bytes() == data
            repeat = run("mirror", "--concurrency", "2")
            assert repeat["downloads"]["progress"]["downloaded"] == 0
            assert repeat["final_check"]["matching_size"] == 2
            # Prove the deployed CLI accepts large sample counts without making
            # any PyPI requests: this local index contains one fixture project.
            result = subprocess.run([
                str(args.binary.resolve()), "mirror", "--sample", "10000",
                "--mirror", str(root), "--index-url", f"http://127.0.0.1:{server.server_port}/simple/",
                "--metadata-url", f"http://127.0.0.1:{server.server_port}/simple/", "--json"
            ], capture_output=True, text=True, timeout=30)
            if result.returncode:
                raise RuntimeError(result.stderr)
            large = json.loads(result.stdout)
            assert len(large["inspection"]["selected_packages"]) == 1
            assert large["downloads"]["progress"]["downloaded"] == 0
            assert sum(path.lstrip("/") in bodies for path in requests) == 2
            print("PASS: missing/mismatch checks, concurrent downloads, integrity, saved reports, repeat-run skipping, and --sample 10000")
    finally:
        server.shutdown()
        server.server_close()
        thread.join()


if __name__ == "__main__":
    main()
