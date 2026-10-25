#!/usr/bin/env python3
"""Linux PTY smoke test: downloads must finish while later metadata is pending."""
import argparse
import errno
import fcntl
import hashlib
import http.server
import json
import os
from pathlib import Path
import pty
import select
import struct
import subprocess
import tempfile
import termios
import threading
import time


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--temp-root", type=Path)
    parser.add_argument("--capture", type=Path)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="snatchrs-pipeline-", dir=args.temp_root) as directory:
        root = Path(directory)
        body = b"pipeline fixture\n" * 65536
        observed = threading.Event()

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                if self.path == "/pypi/early/json":
                    data = {"info": {"name": "early"}, "releases": {"1.0": [{
                        "filename": "early-1.0.whl", "size": len(body),
                        "url": f"http://127.0.0.1:{self.server.server_port}/artifact",
                        "digests": {"sha256": hashlib.sha256(body).hexdigest()}
                    }]}}
                    payload = json.dumps(data).encode()
                elif self.path == "/pypi/later/json":
                    deadline = time.monotonic() + 10
                    while time.monotonic() < deadline:
                        try:
                            snapshot = json.loads((root / "mirror-progress.json").read_text())
                            if ((root / "packages/early/early-1.0.whl").is_file()
                                    and snapshot["downloaded"] == 1
                                    and snapshot["discovery"]["checked_packages"] == 1
                                    and not snapshot["discovery"]["finished"]):
                                observed.set()
                                break
                        except (OSError, KeyError):
                            pass
                        time.sleep(0.02)
                    payload = b'{"info":{"name":"later"},"releases":{}}'
                elif self.path == "/artifact":
                    payload = body
                else:
                    self.send_error(404)
                    return
                self.send_response(200)
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

            def log_message(self, *_):
                pass

        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 140, 0, 0))
        process = subprocess.Popen([
            str(args.binary.resolve()), "mirror", "--mirror", str(root),
            "--package", "early", "--package", "later", "--concurrency", "2",
            "--metadata-url", f"http://127.0.0.1:{server.server_port}/pypi/"
        ], stdin=subprocess.DEVNULL, stdout=slave, stderr=slave,
            env={**os.environ, "TERM": "xterm-256color"})
        os.close(slave)
        transcript = bytearray()
        try:
            deadline = time.monotonic() + 30
            while time.monotonic() < deadline:
                ready, _, _ = select.select([master], [], [], 0.1)
                if ready:
                    try:
                        chunk = os.read(master, 65536)
                    except OSError as error:
                        if error.errno == errno.EIO:
                            break
                        raise
                    if not chunk:
                        break
                    transcript.extend(chunk)
                elif process.poll() is not None:
                    break
            if process.poll() is None:
                process.wait(timeout=2)
            text = transcript.decode(errors="replace")
            if args.capture:
                args.capture.write_text(text)
            assert process.returncode == 0, text
            assert observed.is_set(), "artifact and live progress did not precede later metadata"
            assert "Checking and downloading" in text and "1/2 packages" in text
            assert "queued" in text and "total still growing" in text
            report = json.loads((root / "mirror-report.json").read_text())
            assert report["final_check"]["matching_size"] == 1
            assert report["downloads"]["progress"]["discovery"]["finished"]
            assert (root / "packages/early/early-1.0.whl").read_bytes() == body
            print("PASS: artifact published and progress saved while later metadata was blocked; combined TUI rendered")
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()
            os.close(master)
            server.shutdown()
            server.server_close()
            thread.join()


if __name__ == "__main__":
    main()
