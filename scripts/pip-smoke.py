#!/usr/bin/env python3
"""Exercise a real pip against a static snatchrs mirror; fixtures use stdlib only."""
import argparse
import base64
from contextlib import contextmanager
import csv
import hashlib
import http.server
import io
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile
import threading
from urllib.parse import quote, unquote, urlsplit
import zipfile


PROJECT = "snatchrs-pip-smoke"
VERSION = "1.0+local"
WHEEL = f"snatchrs_pip_smoke-{VERSION}-py3-none-any.whl"


def make_wheel():
    info = f"snatchrs_pip_smoke-{VERSION}.dist-info"
    files = {
        "snatchrs_pip_smoke/__init__.py": b'VALUE = "local mirror fixture"\n',
        f"{info}/METADATA": (
            f"Metadata-Version: 2.1\nName: {PROJECT}\nVersion: {VERSION}\nRequires-Python: >=3.8\n\n"
        ).encode(),
        f"{info}/WHEEL": b"Wheel-Version: 1.0\nGenerator: snatchrs-pip-smoke\nRoot-Is-Purelib: true\nTag: py3-none-any\n",
    }
    record = io.StringIO()
    rows = csv.writer(record, lineterminator="\n")
    for name, data in files.items():
        digest = base64.urlsafe_b64encode(hashlib.sha256(data).digest()).rstrip(b"=").decode()
        rows.writerow([name, f"sha256={digest}", len(data)])
    rows.writerow([f"{info}/RECORD", "", ""])
    files[f"{info}/RECORD"] = record.getvalue().encode()
    output = io.BytesIO()
    with zipfile.ZipFile(output, "w", compression=zipfile.ZIP_DEFLATED) as archive:
        for name, data in files.items():
            entry = zipfile.ZipInfo(name, date_time=(2026, 1, 1, 0, 0, 0))
            entry.compress_type = zipfile.ZIP_DEFLATED
            entry.external_attr = 0o644 << 16
            archive.writestr(entry, data)
    return output.getvalue()


@contextmanager
def serving(handler):
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield server
    finally:
        server.shutdown()
        server.server_close()
        thread.join()


def run(command, *, env=None):
    result = subprocess.run(command, capture_output=True, text=True, timeout=60, env=env)
    if result.returncode:
        raise RuntimeError(f"{shlex.join(map(str, command))}\n{result.stdout}{result.stderr}")
    return result.stdout


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--pip", default="python3 -m pip", help="pip executable or command, e.g. '.venv/bin/python -m pip'")
    parser.add_argument("--temp-root", type=Path)
    args = parser.parse_args()
    try:
        pip = shlex.split(args.pip)
        if not pip:
            raise ValueError("--pip must specify an executable or command")
        # Prevent inherited index/proxy configuration from turning this into an
        # external-network test. pip's fixture and all artifacts are loopback-only.
        env = {key: value for key, value in os.environ.items() if not key.startswith("PIP_")}
        env.update(PIP_CONFIG_FILE=os.devnull, PIP_NO_INPUT="1", NO_PROXY="127.0.0.1,localhost", no_proxy="127.0.0.1,localhost")
        run([*pip, "--version"], env=env)
        wheel = make_wheel()
        checksum = hashlib.sha256(wheel).hexdigest()

        class Origin(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                path = unquote(urlsplit(self.path).path)
                if path == "/simple/":
                    data = {"meta": {"api-version": "1.4"}, "projects": [{"name": PROJECT, "_last-serial": 1}]}
                    body = json.dumps(data).encode()
                elif path == f"/simple/{PROJECT}/":
                    data = {"name": PROJECT, "meta": {"api-version": "1.4", "_last-serial": 1}, "versions": [VERSION], "files": [{
                        "filename": WHEEL, "size": len(wheel), "hashes": {"sha256": checksum}, "requires-python": ">=3.8",
                        "url": f"http://127.0.0.1:{self.server.server_port}/artifacts/{quote(WHEEL, safe='')}",
                    }]}
                    body = json.dumps(data).encode()
                elif path == f"/artifacts/{WHEEL}":
                    body = wheel
                else:
                    self.send_error(404)
                    return
                self.send_response(200)
                self.send_header("Content-Type", "application/octet-stream" if path.startswith("/artifacts/") else "application/vnd.pypi.simple.v1+json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *_):
                pass

        with tempfile.TemporaryDirectory(prefix="snatchrs-pip-smoke-", dir=args.temp_root) as directory:
            root = Path(directory) / "mirror"
            with serving(Origin) as origin:
                report = json.loads(run([
                    str(args.binary.resolve()), "mirror", "--mirror", str(root), "--sample", "1", "--seed", "42",
                    "--index-url", f"http://127.0.0.1:{origin.server_port}/simple/", "--json", "--quiet",
                ], env=env))
                assert report["simple_index"] == {"projects": 1, "files_linked": 1, "files_skipped": 0}, report
            # The origin has stopped: pip can only succeed via generated static
            # pages and mirrored bytes, never by following an upstream URL.
            requests = []

            class Static(http.server.SimpleHTTPRequestHandler):
                def __init__(self, *args, **kwargs):
                    super().__init__(*args, directory=str(root), **kwargs)

                def do_GET(self):
                    requests.append(unquote(urlsplit(self.path).path))
                    super().do_GET()

                def log_message(self, *_):
                    pass

            with serving(Static) as static:
                destination = Path(directory) / "downloads"
                run([
                    *pip, "download", "--no-deps", "--no-cache-dir", "--disable-pip-version-check", "--only-binary=:all:",
                    "--index-url", f"http://127.0.0.1:{static.server_port}/simple/", "--trusted-host", "127.0.0.1",
                    "--dest", str(destination), f"{PROJECT}=={VERSION}",
                ], env=env)
                downloaded = destination / WHEEL
                assert downloaded.is_file(), "pip did not save the fixture wheel"
                assert downloaded.read_bytes() == wheel, "pip downloaded different bytes"
                assert hashlib.sha256(downloaded.read_bytes()).hexdigest() == checksum
                assert f"/simple/{PROJECT}/" in requests, requests
                assert f"/packages/{PROJECT}/{WHEEL}" in requests, requests
            print("PASS: real pip downloaded the hashed +local wheel through static Simple HTML with the origin offline")
        return 0
    except (OSError, ValueError, AssertionError, RuntimeError, subprocess.TimeoutExpired) as error:
        print(f"FAIL: {error}", file=sys.stderr)
        print("If pip is unavailable, seed a venv with uv venv --seed and pass --pip '<venv>/bin/python -m pip'.", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
