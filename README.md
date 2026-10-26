# snatchrs

Build a PyPI mirror with package files and a static Simple index. Serve it to pip
and uv with any static web server, including on offline and air-gapped networks.

- Mirror all versions of the projects you choose.
- Check PyPI's SHA256 hashes while downloading, when supplied.
- Rerun to fetch new, missing, or wrong-sized files.
- Interrupt safely and rerun to continue.
- Deploy a single binary.

**Not included:** dependency resolution, version filtering, a caching proxy, or
JSON index serving. The public index is HTML Simple API only.

## Install

Install Rust 1.89+ and Git, then run:

```sh
git clone https://github.com/rstrong/snatchrs && cd snatchrs
cargo install --locked --path .
export PATH="$HOME/.cargo/bin:$PATH"
```

This installs `snatchrs` in `~/.cargo/bin`. The test below uses Python 3 and pip;
uv is needed only for the uv examples.
<!-- TODO(release): add prebuilt binary download once the release workflow exists -->

## Quick start

1. Mirror packaging. This creates package files, saved metadata, and index pages.

   ```sh
   snatchrs mirror ./mirror --package packaging
   ```

2. Serve it for a test. Leave this running while you try step 3.

   ```sh
   python3 -m http.server 8000 --directory ./mirror
   ```

   This exposes the whole directory and is only for testing. Use the
   [production configuration](#serve-it-properly) below for a shared server.

3. In another terminal, download from the mirror with pip, or install with uv.

   ```sh
   pip download --index-url http://localhost:8000/simple/ --no-deps -d /tmp/snatchrs-try packaging
   uv pip install --index-url http://localhost:8000/simple/ --no-deps --target /tmp/snatchrs-try-uv packaging
   ```

## Mirror what your project needs

snatchrs does **not** resolve dependencies. Name each project you need; it mirrors
**every version** of each name. Use uv to resolve a requirements file first:

```sh
printf 'requests\n' > requirements.in
uv pip compile requirements.in -o requirements.txt
snatchrs check ./mirror --packages-file requirements.txt --json > check.json
snatchrs mirror ./mirror --packages-file requirements.txt
```

`check.json` contains counts and total expected bytes in `summary.expected_bytes`,
without downloading files. That total includes files already present. From a
working environment, `pip freeze > requirements.txt` is another way to list names.
Pins, extras, and markers are ignored: all versions are mirrored. Hash-pinned
lockfiles are not supported; compile without `--generate-hashes`.

One measured requests + flask dependency set was 12 projects, 4,736 files, and
about 790 MiB; your selection will differ. Repeat `--package NAME` for explicit
names. `--all` selects all PyPI, which needs **many tens of TB**. See
[package selection](docs/selecting-packages.md) for the four selection modes.

## Keep it up to date

Rerun the same mirror command. It downloads new, missing, or wrong-sized files
and rebuilds the pages. Use [scripts/run-mirror.sh](scripts/run-mirror.sh) from
cron to record an exit status; see [scheduling](docs/operations.md#long-runs).
Ctrl-C stops cleanly and saves progress reports; rerun to continue.

| Exit | Meaning |
| --- | --- |
| 0 | Success. |
| 1 | Something failed; reports are saved when possible. |
| 2 | Command-line usage error. |
| 130 | Interrupted. |

`check` returns 1 for package errors or blocked paths, but missing or wrong-sized
files alone do not fail it. See [failure handling](docs/operations.md).

## Move it to an air-gapped network

Copy the whole mirror. Replace `/media/usb` with your mounted transfer drive:

```sh
rsync -a ./mirror/ /media/usb/mirror/
snatchrs verify /media/usb/mirror
```

Run `verify` and serve the directory on the offline side. Verification is offline
and needs a writable mirror. Later rsync runs transfer only changes. Serving needs
`simple/` and `packages/`; verification and index rebuilds also need `metadata/`.
Copy everything except, optionally, `reports/`; old reports are not required.

## Serve it properly

Replace the example hostname and `/srv/pypi` with your host and mirror path.
This nginx configuration exposes only the two public trees:

```nginx
server {
    listen 80;
    server_name mirror.example.org;
    root /srv/pypi;
    index index.html;
    location /simple/ { try_files $uri $uri/ =404; }
    location /packages/ { try_files $uri =404; }
    location / { return 404; }
}
```

For pip, put this in `~/.config/pip/pip.conf`, using your actual hostname:

```ini
[global]
index-url = http://mirror.example.org/simple/
trusted-host = mirror.example.org
```

For HTTPS with a valid certificate, use `https://` and omit `trusted-host`.
See [serving](docs/serving.md) for permissions, HTTPS, and uv configuration.

## Commands

| Command | Purpose |
| --- | --- |
| `mirror` | Download selected projects and rebuild their index pages. |
| `check` | Inspect selected projects without downloading files. |
| `verify` | Check saved file sizes and SHA256 hashes offline. |
| `simple` | Rebuild index pages from local metadata, offline. |
| `index` | List projects from the upstream index. |

For `mirror`, `-j/--concurrency` defaults to 8 and `--min-free` keeps `1G` free.
For `verify`, `-j` defaults to 4. `--quiet` suppresses status; `--no-progress`
disables the dashboard. `--json` is available on every command except `simple`.
Use `snatchrs <command> --help` for all options and required selections.

## Documentation

- [Selecting packages](docs/selecting-packages.md): names, files, sampling, caches.
- [Serving](docs/serving.md): nginx, pip, uv, permissions, and HTTPS.
- [Operations](docs/operations.md): disk space, retries, proxies, repair, scheduling.
- [Reports](docs/reports.md): layout, JSONL fields, and retention.
- [How it works](docs/how-it-works.md): inspection, downloads, and integrity checks.
- [Development](docs/development.md): building, testing, and copying a binary.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this project by you, as defined in the Apache-2.0 license, shall
be dual licensed as above, without any additional terms or conditions.
