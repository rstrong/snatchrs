# snatchrs

snatchrs copies the PyPI projects you choose into a plain directory. Any static web
server can then serve that directory to pip and uv. It is built for offline and
air-gapped networks.

- It mirrors every version of each project you name.
- It checks each file against PyPI's SHA256 hash while downloading, when PyPI
  provides one.
- A rerun downloads only new, missing or wrong-sized files.
- You can stop it at any time and rerun it to continue.
- It is a single binary.

**Not included:** dependency resolution, version filtering, or a caching proxy.
The mirror serves the HTML Simple API only, not the JSON one.

## Install

You need Git and Rust 1.89 or newer.

```sh
git clone https://github.com/rstrong/snatchrs && cd snatchrs
cargo install --locked --path .
export PATH="$HOME/.cargo/bin:$PATH"
```

This puts `snatchrs` in `~/.cargo/bin`.
<!-- TODO(release): add prebuilt binary download once the release workflow exists -->

## Quick start

1. Mirror one project:

   ```sh
   snatchrs mirror ./mirror --package packaging
   ```

   This downloads every release of `packaging` into `./mirror` and writes the
   index pages.

2. Serve the mirror for a quick test, and leave it running:

   ```sh
   python3 -m http.server 8000 --directory ./mirror
   ```

   This serves the whole directory, so use it only for testing. For a shared
   server, see [Serve it properly](#serve-it-properly).

3. In a second terminal, download from the mirror with pip or uv:

   ```sh
   pip download --index-url http://localhost:8000/simple/ --no-deps -d /tmp/snatchrs-try packaging
   uv pip install --index-url http://localhost:8000/simple/ --no-deps --target /tmp/snatchrs-try-uv packaging
   ```

## Mirror what your project needs

snatchrs does **not** resolve dependencies. It mirrors only the projects you
name, and it mirrors **every version** of each one. To get the full list for a
project, resolve it with uv first:

```sh
printf 'requests\n' > requirements.in
uv pip compile requirements.in -o requirements.txt
snatchrs check ./mirror --packages-file requirements.txt
snatchrs mirror ./mirror --packages-file requirements.txt
```

`check` looks up the projects without downloading anything. Its last line
shows how many files, and how many bytes, `mirror` would download.

Things to know about the package list:

- `pip freeze > requirements.txt` in a working environment also gives you a list.
- Version pins, extras and markers are ignored. Every version is mirrored.
- Hash-pinned lockfiles work. The hashes are ignored.

Sizes add up quickly. `requests` and `flask` with their dependencies came to 12
projects, 4,736 files and about 790 MiB.

To name projects directly, repeat `--package NAME`. `--all` mirrors all of PyPI,
which is **many tens of terabytes**. See
[Selecting packages](docs/selecting-packages.md) for every option.

## Keep it up to date

Rerun the same `mirror` command. It downloads only new, missing or wrong-sized
files, then rebuilds the index pages. To run it from cron, use
[scripts/run-mirror.sh](scripts/run-mirror.sh), which records the exit status; see
[Long runs](docs/operations.md#long-runs).

Ctrl-C stops a run cleanly and saves its reports. Rerun to continue.

| Exit | Meaning |
| --- | --- |
| 0 | Success. |
| 1 | Something failed. Reports are saved when possible. |
| 2 | The command line was invalid. |
| 130 | Interrupted. |

`check` exits 0 when files are only missing or the wrong size. It exits 1 for
project errors or blocked paths. See
[Failures and exit codes](docs/operations.md#failures-and-exit-codes).

## Move it to an air-gapped network

Copy the whole mirror directory. Replace `/media/usb` with your transfer drive:

```sh
rsync -a ./mirror/ /media/usb/mirror/
snatchrs verify /media/usb/mirror
```

On the offline side, run `verify` and serve the directory. `verify` works
offline, but it writes a report, so the mirror must be writable. Later `rsync`
runs copy only what changed.

| Directory | Needed for |
| --- | --- |
| `simple/`, `packages/` | Serving to pip and uv. |
| `metadata/` | `verify`, and rebuilding the index with `simple`. |
| `reports/` | Nothing. Copying it is optional. |

## Serve it properly

This nginx configuration exposes only the index and package files. Replace the
hostname and `/srv/pypi` with your own:

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

Then point pip at it in `~/.config/pip/pip.conf`:

```ini
[global]
index-url = http://mirror.example.org/simple/
trusted-host = mirror.example.org
```

With HTTPS and a valid certificate, use `https://` and drop `trusted-host`. See
[Serving](docs/serving.md) for file permissions, HTTPS and uv.

## Commands

| Command | What it does |
| --- | --- |
| `mirror` | Downloads the selected projects and rebuilds their index pages. |
| `check` | Looks up the selected projects and compares them with the mirror. Downloads nothing. |
| `verify` | Checks file sizes and SHA256 hashes, offline. |
| `simple` | Rebuilds the index pages from saved metadata, offline. |
| `index` | Lists every project on the upstream index. |

Common options:

| Option | Effect |
| --- | --- |
| `-j N` | Parallel downloads for `mirror` (default 8) or hashing for `verify` (default 4). |
| `--min-free SIZE` | Free disk space `mirror` keeps in reserve (default `1G`). |
| `--quiet` | Hides status messages. Results and errors still print. |
| `--no-progress` | Prints plain status lines instead of the live display. |
| `--json` | Prints the result as JSON. Not available on `simple`. |

Run `snatchrs <command> --help` for the full list.

## Documentation

- [Selecting packages](docs/selecting-packages.md): package lists, sampling and the metadata cache.
- [Serving](docs/serving.md): nginx, pip, uv, permissions and HTTPS.
- [Operations](docs/operations.md): disk space, retries, proxies, repair and scheduling.
- [Reports](docs/reports.md): the directory layout, report fields and retention.
- [How it works](docs/how-it-works.md): the download pipeline and integrity checks.
- [Development](docs/development.md): building, testing and copying a binary.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this project by you, as defined in the Apache-2.0 license, shall
be dual licensed as above, without any additional terms or conditions.
