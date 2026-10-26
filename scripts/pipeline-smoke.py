#!/usr/bin/env python3
"""Linux PTY tests for overlap and checking ahead of blocked downloads."""
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
    parser.add_argument("--check-ahead", action="store_true", help="Hold every worker until checking and queueing finish")
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="snatchrs-pipeline-", dir=args.temp_root) as directory:
        root = Path(directory)
        body = b"pipeline fixture\n" * (1024 if args.check_ahead else 65536)
        observed = threading.Event()
        if args.check_ahead:
            existing = root / "packages/existing"
            existing.mkdir(parents=True)
            (existing / "existing.whl").write_bytes(b"present")

        def metadata(name, filenames, data):
            return json.dumps({"name": name, "meta": {"api-version": "1.4"}, "versions": ["1.0"], "files": [
                {"filename": filename, "size": len(data),
                 "url": f"http://127.0.0.1:{server.server_port}/artifact/{filename}",
                 "hashes": {"sha256": hashlib.sha256(data).hexdigest()}}
                for filename in filenames
            ]}).encode()

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                if self.path == "/simple/early/":
                    names = [f"early-{index}.whl" for index in range(20)] if args.check_ahead else ["early-1.0.whl"]
                    payload = metadata("early", names, body)
                elif self.path == "/simple/existing/":
                    payload = metadata("existing", ["existing.whl"], b"present")
                elif self.path == "/simple/later/" and args.check_ahead:
                    payload = metadata("later", ["later.whl"], body)
                elif self.path == "/simple/later/":
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
                    payload = b'{"name":"later","meta":{"api-version":"1.4"},"versions":[],"files":[]}'
                elif self.path.startswith("/artifact/"):
                    if args.check_ahead:
                        deadline = time.monotonic() + 10
                        while not observed.is_set() and time.monotonic() < deadline:
                            try:
                                snapshot = json.loads((root / "mirror-progress.json").read_text())
                                if (snapshot["discovery"]["finished"]
                                        and snapshot["discovery"]["checked_packages"] == 3
                                        and snapshot["total_files"] == 21
                                        and snapshot["downloaded"] == 0):
                                    observed.set()
                                    break
                            except (OSError, KeyError):
                                pass
                            time.sleep(0.02)
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
        packages = ["early", "existing", "later"] if args.check_ahead else ["early", "later"]
        selection = [arg for name in packages for arg in ("--package", name)]
        process = subprocess.Popen([
            str(args.binary.resolve()), "mirror", str(root),
            *selection, "--concurrency", "2",
            "--metadata-url", f"http://127.0.0.1:{server.server_port}/simple/"
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
            assert observed.is_set(), "checking and downloading did not advance independently"
            assert "Checking and downloading" in text
            assert ("3/3 packages" if args.check_ahead else "1/2 packages") in text
            assert "queued" in text and "total still growing" in text
            report = json.loads((root / "mirror-report.json").read_text())
            assert report["final_check"]["matching_size"] == (22 if args.check_ahead else 1)
            assert report["downloads"]["progress"]["discovery"]["finished"]
            if args.check_ahead:
                assert report["downloads"]["progress"]["downloaded"] == 21
                assert report["inspection"]["summary"]["matching_size"] == 1
                assert (root / "packages/existing/existing.whl").read_bytes() == b"present"
                for file in (root / "packages/early").iterdir():
                    assert file.read_bytes() == body
                assert (root / "packages/later/later.whl").read_bytes() == body
                print("PASS: all 3 packages checked, existing file verified, and 21 jobs queued before any blocked download completed; TUI rendered")
            else:
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
