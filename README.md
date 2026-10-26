# snatchrs

A Rust starting point for a PyPI mirror, with a CLI inspired by uv:
subcommands, concise status messages on stderr, pipeable results on stdout,
`--quiet`, `--help`, and `--version`. This program should help build and maintain
pypi mirrors for offline and air-gapped networks, similar to bandersnatch, but
with the goal of being simple to deploy and maintain.

## Quick start

Install stable Rust, then run these commands from the project directory:

```bash
# Build the binary.
cargo build --release --locked

# Check one package against local files, without downloading anything.
./target/release/snatchrs check --mirror ./mirror-shell --package packaging

# Download its missing or wrong-sized files with four concurrent workers.
./target/release/snatchrs mirror --mirror ./mirror-shell --package packaging --concurrency 4
```

Files and metadata are saved under `./mirror-shell`, with live terminal progress.
All releases of the selected package are included. To select 100 random packages,
replace `--package packaging` with `--sample 100`.

In an interactive terminal, `snatchrs index` displays an animated dashboard:
bytes read while decoding JSON, followed by a package progress bar, processed and
total counts, elapsed time, and the current package name. Counts come from actual
work; the total is known only after the root JSON has been decoded. The dashboard
refreshes at most every 80 ms, so it samples names instead of printing every one.
Use `--no-progress` to print package names in a terminal. Redirecting stdout or
using `--json` also selects plain output automatically. `--quiet` suppresses status.

```text
⠹ snatchrs · Building packages  00:00:08
  ━━━━━━━━━━━━━━━━━━━╸──────────────────────  48%
  480 / 1,000 packages
  Current: example-project
```

## Run

Install stable Rust (1.89 or newer, for `File::try_lock`), then:

```bash
cargo run -- index
cargo run -- index --json > index.json
cargo run -- index --index-url https://test.pypi.org/simple/
```

The live command requests the complete project list once, so expect a large
response. It does not request individual package pages or download distributions.
The `index` command does not persist results itself; `check` persists package
metadata and a report. Network requests use the retry policy below. Incremental
synchronization is not implemented yet.

To try the parser with three packages and no network requests:

```bash
cargo run -- index --from-file tests/fixtures/index.json
cargo run -- --quiet index --from-file tests/fixtures/index.json --json
```

`--from-file` accepts a raw Simple Index API JSON response, not this tool's
enriched `--json` output. `--index-url` determines the package URLs when reading
a fixture.

## Package model

`PackageIndex` owns a `Vec<Package>` and supports iteration by reference. Each
`Package` contains the original name, normalized name, package Simple Index URL,
and optional PyPI last-change serial. The index also preserves its API version
and optional repository serial.

```rust,no_run
use snatchrs::{IndexClient, DEFAULT_INDEX_URL};

fn main() -> anyhow::Result<()> {
    let index = IndexClient::new()?.fetch(DEFAULT_INDEX_URL.parse()?)?;
    for package in &index {
        println!("{}: {}", package.normalized_name, package.index_url);
        // Add validation or per-package metadata loading here next.
    }
    Ok(())
}
```

The parser uses the [JSON Simple Index API](https://docs.pypi.org/api/index-api/),
which PyPI recommends for new integrations, and follows the
[PyPA name normalization rules](https://packaging.python.org/en/latest/specifications/name-normalization/).
It accepts API major version 1 and ignores unknown response fields.
Malformed names and malformed responses produce errors instead of being skipped.
The root index supplies discovery data. The `check` command enriches selected
packages with release and file metadata as described below.

## Inspect a mirror shell

```bash
# Discover all project names, then inspect ten random packages (the default).
cargo run --release -- check --mirror mirror-shell

# Record/repeat the random selection, given the same root index.
./target/release/snatchrs check --mirror mirror-shell --sample 10 --seed 42

# Recheck specific projects without fetching the root index.
./target/release/snatchrs check --mirror mirror-shell --package requests --package packaging

# Emit the summary and detail-file paths as well as saving them in the mirror.
./target/release/snatchrs check --mirror mirror-shell --package packaging --json > report.json
```

This command makes one root index request when sampling, then one metadata request
per selected project. It never requests or downloads distribution files. A sample
is without replacement; if the index has fewer packages than requested, all are
selected. The report records the random seed and exact names. Seeds reproduce
selection only for an unchanged index; use repeated `--package` arguments to
recheck the exact names after PyPI changes.

The terminal dashboard shows the current project, packages completed, release and
file counts, missing files, and size mismatches. Plain output remains available
with `--no-progress`, `--quiet`, or redirected stdout.

The mirror layout is:

```text
mirror-shell/
  metadata/<normalized-name>.json       # enriched package/release/file metadata
  packages/<normalized-name>/           # empty initially
    <distribution-filename>             # expected future artifact location
  check-report.json                    # latest run summary and detail-file paths
```

All filesystem paths stored in metadata, reports, JSONL details, and download
outcomes are relative to the mirror root, such as
`packages/packaging/packaging-25.0.tar.gz` or `reports/run-*/checked-packages.jsonl`.
Resolve them against the directory passed to `--mirror`, rather than the working
directory or the JSON file's own directory. Reports use `mirror_root: "."`.
Moving or copying the whole mirror preserves cached metadata, report links, and
repeat-run file checks. Download URLs remain absolute HTTP/HTTPS URLs.

Legacy metadata caches containing absolute paths are fetched again when selected
and replaced with relative paths; existing distribution files are still checked
and reused. Historical reports from older versions are not rewritten.
`PackageMetadata` contains the package, metadata URL, metadata file path, package
directory, last serial, and every release version. `Release` contains all its
distribution files (all wheels and source distributions, including yanked files).
Empty releases advertised by the index are preserved. Versions are ordered lexically, not by Python
version precedence. Each `DistributionFile` contains filename, download URL, local
path, expected byte size, optional SHA256, distribution type, Python requirement,
and yanked state.

`check` and `mirror` use the per-project
[Simple JSON API](https://docs.pypi.org/api/index-api/#get-distributions-for-project):
`<index-url>/<normalized-project>/` with
`Accept: application/vnd.pypi.simple.v1+json`. Both the project listing and package
metadata follow `--index-url` by default. `--metadata-url` optionally overrides
the Simple Index base for project metadata; it takes a Simple API base URL.

On sampled runs, `check` and `mirror` reuse saved project metadata when the current
root listing's `_last-serial` matches the cached project's serial. Caches are bound
to both the root-listing project URL and the requested metadata URL (before
redirects). Missing, older-format, malformed, or invalid caches are fetched again;
changed serials also trigger a fetch. The project's `X-PyPI-Last-Serial` header is
used when its JSON body omits the serial. A missing serial never counts as a match.
Local files are checked every run, including cache hits, so interrupted downloads,
deleted files, and wrong sizes are still queued for repair.

Use `--refresh` to force fresh metadata for every selected project:

```bash
./target/release/snatchrs mirror --mirror ./mirror-shell --sample 10000 --refresh
```

Both commands fetch project metadata with a bounded worker pool:
`--metadata-concurrency` defaults to 4 and accepts 1–32. It is independent of
`mirror --concurrency`, which controls artifact downloads. Results are checked,
journaled, and queued as they finish, so a slow request or retry does not block
other metadata workers. Completed metadata is buffered in a bounded channel;
large samples do not accumulate all results in memory. Lower the metadata worker
count if individual projects have very large metadata responses.

```bash
./target/release/snatchrs mirror --mirror ./mirror-shell --sample 10000 --metadata-concurrency 8 --concurrency 16
```

Detail records and plain package output follow completion order. The summary's
`selected_packages` list still records the original selection order, and
`metadata_concurrency` records the configured limit. Use
`--metadata-concurrency 1` for sequential metadata requests.

Explicit `--package` selection does not fetch the root listing and therefore
always fetches metadata. Failed refreshes report an error and preserve the previous
cache; they do not silently fall back to potentially stale metadata.

A project's metadata endpoint returning HTTP 404 is recorded separately as
`not_found`, counted in `unavailable_count`, and skipped without retrying. This
covers deleted projects still present in a cached root listing and names that do
not exist. Such outcomes do not increment `error_count` or cause a nonzero exit;
other projects are still checked and downloaded. An observed 404 invalidates only
that project's metadata cache, preserving local distribution files. Other HTTP
errors, a 404 for the root index, and missing distribution downloads remain errors.

The API's flat `files` list supplies URLs, sizes, `hashes`, `requires-python`, and
boolean or string `yanked` values. Relative file URLs are resolved against the
final response URL, including redirects. Files are grouped into the advertised
`versions` using their filenames, with PEP 440 equivalence for normalized version
spellings and support for legacy egg/installer names. Distribution type is inferred
from the filename. Ambiguous or unassignable files fail explicitly rather than
being silently omitted. If an older response omits `versions`, versions are
inferred from recognizable filenames. Missing file sizes fail explicitly, since
size checks require them. There is no fallback to the deprecated project JSON API.

File inspection records `missing`, `size_mismatch` (including actual size),
`matching_size`, `not_regular_file`, or `error`. A matching size is only a size
check; hashes are retained but not verified yet. Existing distributions are never
modified. Unsafe filenames and symlinked package directories are rejected.

`check` never downloads files; missing and size-mismatched files are counted in the
report summary. Blocked paths are reported separately.
Package failures do not prevent inspection of the remaining selection, and are
included in the saved report. Package failures or blocked files produce a nonzero
exit status; ordinary missing/size-mismatched files do not.

## Download a test mirror

```bash
cargo build --release --locked
./target/release/snatchrs mirror --mirror ./mirror-shell --sample 100 --concurrency 8
./target/release/snatchrs mirror --mirror ./mirror-shell --sample 100 --seed 42 -j 4
./target/release/snatchrs mirror --mirror ./mirror-shell --package packaging --threads 2
```

`mirror` defaults to **100 randomly sampled package names**. `--sample` controls
the requested count without a package-count cap; for example, `--sample 10000`
selects 10,000 names (or all names if the index is smaller). Every available version and distribution of those
projects is included. `--package` selects exact names instead. Sampling can select
projects whose metadata has disappeared; these are recorded as errors while the
remaining selected packages are processed, without drawing replacement packages.

`--concurrency` (`-j`, alias `--threads`) controls simultaneous downloads, from 1
through 64; the default is 8. Metadata discovery is sequential, and overlaps with
downloads: each checked package immediately feeds missing/wrong-sized files into
a disk-backed pending queue. Workers start downloading those files while later
packages are still being fetched and checked. The checker can keep adding jobs
even when every download worker is busy; it does not wait for download slots.
Pending jobs are spooled to a temporary JSON-lines file under the mirror root,
and workers read them in order. The spool is per-run, not a crash-resumable queue;
rerunning checks the files again and skips completed downloads. `--attempts`
controls attempts per index request, metadata request, or file (default 3,
maximum 10, including the initial attempt). Existing files with matching
sizes are skipped. Missing and wrong-sized files are deduplicated by local path
and downloaded by a bounded worker pool. Conflicting metadata for one path fails.

New downloads are streamed to unique temporary files on the target filesystem.
Downloads use a 900-second read-stall timeout, reset whenever response data
arrives, with no total transfer deadline. The connection timeout remains 15 seconds.
Automatic HTTP decompression is disabled. Expected byte size and SHA256 (when
provided by PyPI) must match before a file is atomically published. A failed
replacement leaves the previous file intact. Artifacts are stored as bytes and
are never installed, imported, or extracted. A failed command can be rerun to skip
completed files; partial downloads restart from zero. Interrupted processes may
leave hidden temporary files, which are never treated as completed artifacts.

Workers check and conservatively reserve disk space for active transfers,
including replacements. Pending jobs do not reserve their future artifact size
or block checking while waiting for a worker. Reservations are released after
each attempt. Transfer/storage failures are recorded per file. A queue-spool
write failure stops further discovery, drains accepted jobs, and is recorded in
the final report. Individual metadata failures are recorded and
discovery continues. A filesystem lock prevents simultaneous
`check`/`mirror` commands from using the same root. Existing files are checked by
size only; their hashes are not revalidated on subsequent runs.

In one terminal display, progress includes packages checked and the current name,
metadata errors, unavailable projects, files discovered, queued/active downloads, byte counts, transfer
speed, and a progress line for each worker with package/filename/attempt. Download
totals grow during discovery, so the UI labels them as provisional until checking
finishes. Workers and live progress remain responsive during slow metadata requests.
For noninteractive runs, concise progress is logged to stderr every five seconds.
`--no-progress`, `--quiet`, and `--json` work as for the other commands.

Saved files include:

- `check-report.json`: selected names, seed, inspection counters, `error_count`, and paths to detail files; written when discovery ends.
- `mirror-progress.json`: periodically updated checking, worker, and transfer progress.
- `mirror-report.json`: inspection summary, download counters, `downloads.outcomes_path`, and final size checks.
- `reports/run-*/checked-packages.jsonl`: one checked package per line, including metadata and initial file states (`inspection.packages_path`).
- `reports/run-*/package-errors.jsonl`: one metadata/queue error per line (`inspection.errors_path`).
- `reports/run-*/unavailable-projects.jsonl`: one project metadata 404 per line, with name, requested metadata URL, `status: "not_found"`, and `http_status: 404` (`inspection.unavailable_path`).
- `reports/run-*/download-outcomes.jsonl`: one completed download outcome per line, including failures.

Both `check` and `mirror` append details as work completes, rather than retaining
all package metadata or completed download outcomes in memory. Final size checks
read one checked package at a time. `--json` prints the compact summary, using the
same format as the saved report. The previous inline `packages`, `errors`, and
`downloads.outcomes` arrays have been replaced by the JSONL paths above; consumers
should stream those files line by line. Selected names and queue deduplication
fingerprints are still retained in memory. Metadata held by active workers and
the bounded result buffer must also fit in memory.

Each run has its own detail directory. Earlier report paths remain readable after
subsequent runs; completed and interrupted run directories are kept for inspection
and can be removed when their reports are no longer needed. JSONL files may end
with an incomplete line if the process is interrupted; only complete lines should
be consumed from an interrupted run.

Any metadata errors, failed downloads, missing/wrong-sized files, or blocked paths
produce a nonzero exit code after the final report is saved. Reports use atomic
replacement. The tool currently mirrors artifacts and metadata; serving a pip
compatible Simple Index is a later step.

For a 10,000-package download run:

```bash
./target/release/snatchrs mirror --mirror ./mirror-shell --sample 10000 --seed 42 --concurrency 8
```

To verify a copied binary without Rust, the standalone smoke test uses Python's
standard library and a loopback HTTP server. It tests missing/wrong-sized files,
concurrent downloads, integrity, saved reports, and a second run that skips files:

```bash
python3 scripts/remote-smoke.py --binary ./target/release/snatchrs
# Verify overlapping discovery/downloads and render the combined TUI in a PTY:
python3 scripts/pipeline-smoke.py --binary ./target/release/snatchrs
# Verify checking/queueing can finish while every download worker is blocked:
python3 scripts/pipeline-smoke.py --binary ./target/release/snatchrs --check-ahead
# Independently re-read and hash a completed mirror's artifacts:
python3 scripts/verify-mirror.py ./mirror-shell
```

For a long-running mirror, run the command inside your terminal multiplexer.
The optional runner accepts a mirror root followed by any `mirror` options:

```bash
SNATCHRS_BIN=./target/release/snatchrs bash scripts/run-mirror.sh ./mirror-shell --sample 10000 --concurrency 8
```

The runner uses `snatchrs` from `PATH` unless `SNATCHRS_BIN` is provided. It records
`run.status` under the chosen mirror root and does not provision or format disks.
The verification script writes `verification.json`. It reports metadata errors
separately and returns nonzero for artifact integrity errors or download failures.

## Retry policy

`--attempts` applies to index requests, per-project metadata requests, and file
downloads. It defaults to 3 total attempts, including the initial request, and
accepts 1–10. Use `--attempts 1` to disable retries. Library callers can configure
metadata/index attempts with `IndexClient::with_attempts(n)`.

Retryable failures are connection/request timeouts, interrupted connections or
response bodies, and HTTP **408, 429, 500, 502, 503, and 504**. Other HTTP errors
(including 401, 403, and 404), malformed metadata, invalid paths,
insufficient disk space, filesystem errors, and size/SHA256 mismatches fail
immediately. Exhausted metadata failures are recorded once per package and the
checker continues with the remaining selection.

Retries use exponential backoff with jitter: the first wait is 250–500 ms,
doubling up to a 30-second client backoff cap. A valid
[`Retry-After`](https://www.rfc-editor.org/rfc/rfc9110.html#name-retry-after) header
on a retryable response is treated as a minimum wait, accepting either seconds
or an HTTP date. Jittered backoff is added after that server wait, and the server
delay is not shortened by the client cap. Invalid headers fall back to normal
backoff; past dates add no server wait.

File retries restart from zero after removing the failed temporary file and
releasing its disk reservation. Previously published files remain intact. The
TUI and `mirror-progress.json` identify workers waiting to retry and their delay.

```bash
./target/release/snatchrs check --mirror ./mirror-shell --package packaging --attempts 5
./target/release/snatchrs mirror --mirror ./mirror-shell --sample 100 --attempts 5
```

## HTTP and HTTPS proxies

Set proxy environment variables before starting the program. They apply to index
requests, package metadata, and every artifact download:

| Variable | Lowercase equivalent | Purpose |
| --- | --- | --- |
| `HTTP_PROXY` | `http_proxy` | Proxy for HTTP destinations |
| `HTTPS_PROXY` | `https_proxy` | Proxy for HTTPS destinations, using CONNECT |
| `ALL_PROXY` | `all_proxy` | Fallback when no protocol-specific proxy is set |
| `NO_PROXY` | `no_proxy` | Comma-separated hosts or addresses to reach directly |

```bash
export HTTP_PROXY=http://proxy.example.org:3128
export HTTPS_PROXY=http://proxy.example.org:3128
export NO_PROXY=localhost,127.0.0.1

./target/release/snatchrs mirror --mirror ./mirror-shell --sample 100 --concurrency 8
```

Replace the example proxy hostname with your proxy. An `http://` proxy URL in
`HTTPS_PROXY` is normal: CONNECT tunnels the HTTPS connection through the proxy,
and destination TLS certificates are still verified. Proxy URLs can include
authentication credentials. Protocol-specific variables take priority over
`ALL_PROXY`; `NO_PROXY` bypasses the proxy for matching destinations. Proxy
failures are reported as request or download errors.

The clients share the same transport defaults and follow
[reqwest's environment proxy behavior](https://docs.rs/reqwest/0.12.28/reqwest/#proxies).
Proxy regressions run entirely against local test servers, in child processes
with isolated environment variables:

```bash
cargo test --test proxy
```

## Development

```bash
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
```

Tests cover index and metadata fixtures, every release and file, deterministic
sampling, local file states, path validation, JSON/plain CLI output, and HTTP
success/failure handling using local servers. Temporary mirror fixtures exercise
matching, missing, wrong-sized, zero-byte, directory, and symlink cases.

Commit `Cargo.lock` after the first successful Cargo build because this project
includes an application.

## Build and copy a binary

```bash
cargo build --release --locked
./target/release/snatchrs --version
scp target/release/snatchrs user@example.org:~/snatchrs
ssh -t user@example.org './snatchrs index'
```

The release binary is `target/release/snatchrs`; the remote machine does not need
Rust or the source tree. The commands above assume a compatible Linux machine
with the same CPU architecture and compatible glibc. A binary built on a newer
Linux distribution may not run on an older distribution. Use `file target/release/snatchrs`
and `ldd target/release/snatchrs` to inspect the build. For other operating systems,
architectures, or older Linux versions, build on a matching host or use a suitable
cross-compilation toolchain. SSH needs `-t` to allocate a terminal for the dashboard.

For an offline remote smoke test, copy the fixture too:

```bash
scp tests/fixtures/index.json user@example.org:~/snatchrs-fixture.json
ssh -t user@example.org './snatchrs index --from-file ~/snatchrs-fixture.json'
```

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this project by you, as defined in the Apache-2.0 license, shall
be dual licensed as above, without any additional terms or conditions.
