# snatchrs

A Rust starting point for a PyPI mirror, with a CLI inspired by uv:
subcommands, concise status messages on stderr, pipeable results on stdout,
`--quiet`, `--help`, and `--version`.

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

Install stable Rust (edition 2024 requires Rust 1.85 or newer), then:

```bash
cargo run -- index
cargo run -- index --json > index.json
cargo run -- index --index-url https://test.pypi.org/simple/
```

The live command requests the complete project list once, so expect a large
response. It does not request individual package pages or download distributions.
The `index` command does not persist results itself. There is no retry policy or
incremental synchronization yet; `check` persists package metadata and a report.

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

# Emit the complete structured report as well as saving it in the mirror.
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
  check-report.json                    # latest run, selected names, checks, errors
```

Paths stored in the metadata and report are absolute paths under the mirror root.
`PackageMetadata` contains the package, metadata URL, metadata file path, package
directory, last serial, and every release version. `Release` contains all its
distribution files (all wheels and source distributions, including yanked files).
Empty releases are preserved. Versions are ordered lexically, not by Python
version precedence. Each `DistributionFile` contains filename, download URL, local
path, expected byte size, optional SHA256, distribution type, Python requirement,
and yanked state.

`check` uses the [PyPI project JSON API](https://docs.pypi.org/api/json/) because it
provides version-grouped files and sizes in a single response. PyPI deprecates its
`releases` field; this dependency is isolated in `IndexClient::fetch_metadata` so
the metadata source can change later. A response missing releases or a file size
fails explicitly. `--metadata-url` configures this API base independently of
`--index-url`; changing the index alone does not change the metadata server.

File inspection records `missing`, `size_mismatch` (including actual size),
`matching_size`, `not_regular_file`, or `error`. A matching size is only a size
check; hashes are retained but not verified yet. Existing distributions are never
modified. Unsafe filenames and symlinked package directories are rejected.

For `check`, `DownloadQueueStub` only counts missing/size-mismatched candidates. It does not
store jobs, start workers, or download files. Blocked paths are reported separately.
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
controls attempts per file (default 3, maximum 10). Existing files with matching
sizes are skipped. Missing and wrong-sized files are deduplicated by local path
and downloaded by a bounded worker pool. Conflicting metadata for one path fails.

New downloads are streamed to unique temporary files on the target filesystem.
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
metadata errors, files discovered, queued/active downloads, byte counts, transfer
speed, and a progress line for each worker with package/filename/attempt. Download
totals grow during discovery, so the UI labels them as provisional until checking
finishes. Workers and live progress remain responsive during slow metadata requests.
For noninteractive runs, concise progress is logged to stderr every five seconds.
`--no-progress`, `--quiet`, and `--json` work as for the other commands.

Saved files include:

- `check-report.json`: selected names, seed, metadata, and each file's state when checked; written when discovery ends.
- `mirror-progress.json`: periodically updated checking, worker, and transfer progress.
- `mirror-report.json`: inspection, download outcomes/errors, and final size checks.

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
