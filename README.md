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
./target/release/snatchrs check ./mirror --package packaging

# Download its missing or wrong-sized files with four concurrent workers.
./target/release/snatchrs mirror ./mirror --package packaging --concurrency 4
```

Files, metadata, and static HTML Simple Index pages are saved under `./mirror`,
with live terminal progress. See [Serving the mirror](#serving-the-mirror) for nginx,
pip, and uv setup.
All releases of the selected package are included. To select 100 random packages,
replace `--package packaging` with `--sample 100`.

## Choosing what to mirror

`mirror` and `check` require an explicit directory and exactly one selection mode.
There is no default directory or random sample. `simple` also requires a directory.

```bash
snatchrs mirror ./mirror --all
snatchrs mirror ./mirror --package requests --package numpy
snatchrs mirror ./mirror --packages-file packages.txt
snatchrs mirror ./mirror --sample 100 --seed 42
snatchrs check ./mirror --packages-file packages.txt
snatchrs simple ./mirror
```

- `--all` selects every project in root-index order. Full PyPI is **many tens of
  TB**; choose storage and bandwidth accordingly before requesting a full mirror.
- Repeat `--package NAME` for explicit names; normalization and deduplication keep
  the first occurrence. This mode never requests the root index.
- `--packages-file PATH` reads one leading project name per line, ignoring blank
  lines and whole-line or trailing `#` comments. Extras, version constraints,
  markers, and direct URLs after the name are ignored. Names are normalized and
  deduplicated in first-seen order. This mode also skips the root index; an empty
  or comment-only file is an error.
- `--sample N` explicitly requests a random sample; `--seed` is accepted only
  with this mode. Selection is without replacement and limited by the index size.

A packages file can contain requirements-like lines:

```text
requests>=2.0
Foo[bar]==1; python_version<"3.9"
name @ https://example.org/name.whl
```

**All versions** of each named project are mirrored, regardless of constraints,
extras, markers, or URLs in the file. Dependencies are **not resolved**. This is
not a full requirements parser: lines starting with pip options such as `-r` or
`-e`, and lines without a valid project name, fail with their line number.

`mirror DIR --all` treats a successfully fetched root listing as authoritative:
existing project pages absent from that listing and their metadata caches are
removed during the final pass, even when individual metadata requests or downloads
failed. Those failures still produce a nonzero exit, but projects remaining in the
listing are not pruned. Runs that abort before the final pass skip pruning.
Downloaded files under `packages/` are always preserved. Other selection modes
never prune unselected projects, and `check` never changes public Simple Index
pages. Metadata 404s for selected projects retain their existing separate
unavailable-project handling.

The main options are selection, `--seed`, `-j/--concurrency` (default 8), `--quiet`,
and `--no-progress`. Less common controls are under **Advanced** in help:
metadata workers default to 4, attempts default to 3, and `--min-free` defaults to
`1G` of space kept free on the mirror filesystem.

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

snatchrs uses the [JSON Simple Index API](https://docs.pypi.org/api/index-api/)
and follows the [PyPA name normalization rules](https://packaging.python.org/en/latest/specifications/name-normalization/).
It accepts API major version 1 and ignores unknown response fields; malformed
names and responses produce errors instead of being skipped.

## Inspect a mirror shell

```bash
# Discover project names, then explicitly sample ten packages.
cargo run --release -- check ./mirror --sample 10

# Record/repeat the random selection, given the same root index.
./target/release/snatchrs check ./mirror --sample 10 --seed 42

# Recheck specific projects without fetching the root index.
./target/release/snatchrs check ./mirror --package requests --package packaging

# Emit the summary and detail-file paths as well as saving them in the mirror.
./target/release/snatchrs check ./mirror --package packaging --json > report.json
```

This command requests the root index for `--all` and `--sample`, then fetches metadata
per selected project. It never requests or downloads distribution files. A sample
is without replacement; if the index has fewer packages than requested, all are
selected. Sample reports record the random seed and exact selected names. Seeds reproduce
selection only for an unchanged index; use repeated `--package` arguments to
recheck the exact names after PyPI changes.

The terminal dashboard shows the current project, packages completed, release and
file counts, missing files, and size mismatches. Plain output remains available
with `--no-progress`, `--quiet`, or redirected stdout.

The mirror layout is:

```text
./mirror/
  metadata/<normalized-name>.json       # enriched package/release/file metadata
  packages/<normalized-name>/           # empty initially
    <distribution-filename>             # expected future artifact location
  check-report.json                    # latest run summary and detail-file paths
```

All filesystem paths stored in metadata, reports, JSONL details, and download
outcomes are relative to the mirror root, such as
`packages/packaging/packaging-25.0.tar.gz` or `reports/run-*/checked-packages.jsonl`.
Resolve them against the positional mirror directory `DIR`, rather than the working
directory or the JSON file's own directory. Reports use `mirror_root: "."`.
Moving or copying the whole mirror preserves cached metadata, report links, and
repeat-run file checks. Download URLs remain absolute HTTP/HTTPS URLs.

Legacy metadata caches containing absolute paths are fetched again when selected
and replaced with relative paths; existing distribution files are still checked
and reused. Historical reports from older versions are not rewritten.
Saved metadata records the project name, metadata URL and file path, package
directory, last serial, and every release version. Each release lists all wheels
and source distributions, including yanked files. Empty releases advertised by
the index are preserved. Versions are ordered lexically, not by Python version
precedence. Each file records its filename, download URL, local path, expected
byte size, optional SHA256, distribution type, Python requirement, and yanked state.

`check` and `mirror` use the per-project
[Simple JSON API](https://docs.pypi.org/api/index-api/#get-distributions-for-project):
`<index-url>/<normalized-project>/` with
`Accept: application/vnd.pypi.simple.v1+json`. Both the project listing and package
metadata follow `--index-url` by default. `--metadata-url` optionally overrides
the Simple Index base for project metadata; it takes a Simple API base URL.

On `--all` and sampled runs, `check` and `mirror` reuse saved project metadata when the current
root listing's `_last-serial` matches the cached project's serial. Caches are bound
to both the root-listing project URL and the requested metadata URL (before
redirects). Missing, older-format, malformed, or invalid caches are fetched again;
changed serials also trigger a fetch. The project's `X-PyPI-Last-Serial` header is
used when its JSON body omits the serial. A missing serial never counts as a match.
Local files are checked every run, including cache hits, so interrupted downloads,
deleted files, and wrong sizes are still queued for repair.

Use `--refresh` to force fresh metadata for every selected project:

```bash
./target/release/snatchrs mirror ./mirror --sample 10000 --refresh
```

Both commands fetch project metadata with a bounded worker pool:
`--metadata-concurrency` defaults to 4 and accepts 1–32. It is independent of
`mirror --concurrency`, which controls artifact downloads. Results are checked,
journaled, and queued as they finish, so a slow request or retry does not block
other metadata workers. Completed metadata is buffered in a bounded channel;
large samples do not accumulate all results in memory. Lower the metadata worker
count if individual projects have very large metadata responses.

```bash
./target/release/snatchrs mirror ./mirror --sample 10000 --metadata-concurrency 8 --concurrency 16
```

Detail records and plain package output follow completion order. The summary's
`selected_packages` list records the original selection order except for `--all`,
which omits that list. `metadata_concurrency` records the configured limit. Use
`--metadata-concurrency 1` for sequential metadata requests.

`--package` and `--packages-file` do not fetch the root listing and therefore
always fetch metadata. Failed refreshes report an error and preserve the previous
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
from the filename. Release labels are informational: equivalent advertised
spellings are resolved deterministically, and legacy names use the longest
matching version prefix. If no advertised release fits, the filename-derived
version or `unknown` is used, so an odd filename never excludes a file or fails
the project. If an older response omits `versions`, versions are inferred from
recognizable filenames. Missing file sizes fail explicitly, since
size checks require them. There is no fallback to the deprecated project JSON API.

File inspection records `missing`, `size_mismatch` (including actual size),
`matching_size`, `not_regular_file`, or `error`. A matching size is only a size
check; use `snatchrs verify` to validate recorded hashes. Inspection never modifies
existing distributions. Unsafe filenames and symlinked package directories are rejected.

`check` never downloads files; missing and size-mismatched files are counted in the
report summary. Blocked paths are reported separately.
Package failures do not prevent inspection of the remaining selection, and are
included in the saved report. Package failures or blocked files produce a nonzero
exit status; ordinary missing/size-mismatched files do not.

## Download a test mirror

```bash
cargo build --release --locked
./target/release/snatchrs mirror ./mirror --sample 100 --concurrency 8
./target/release/snatchrs mirror ./mirror --sample 100 --seed 42 -j 4
./target/release/snatchrs mirror ./mirror --package packaging -j 2
```

`mirror` requires a directory and an explicit selection; it never picks a random
sample unless `--sample N` is given. Samples have no package-count cap; for
example, `--sample 10000` selects 10,000 names (or all names if the index is
smaller). Every available version and distribution of each selected project is
included. Projects whose metadata returns 404 are recorded as unavailable while
the remaining selection is processed, without drawing replacement packages.

`--concurrency` (`-j`) controls simultaneous downloads, from 1
through 64; the default is 8. Metadata fetching uses four workers by default and
overlaps with downloads: each checked package immediately feeds missing or
wrong-sized files into a disk-backed pending queue. Workers start downloading those files while later
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
Index and metadata requests use a 60-second read-stall timeout; downloads use
900 seconds. These timeouts reset whenever response data arrives, with no total
transfer deadline. The connection timeout is 15 seconds. API/CDN metadata should
respond promptly, while artifact mirrors can be slower.
Automatic HTTP decompression is disabled. Expected byte size and SHA256 (when
provided by PyPI) must match before a file is atomically published. A failed
replacement leaves the previous file intact. Artifacts are stored as bytes and
are never installed, imported, or extracted. A failed command can be rerun to skip
completed files; partial downloads restart from zero.

The first Ctrl-C (also SIGTERM or SIGHUP) stops discovery and hashing, cancels
in-flight downloads, restores the terminal, and saves reports before exiting
with code **130**. Completed files are kept; rerun the same command to continue.
A second signal exits immediately with code 130. `index` retains its default
immediate signal handling.

After taking the mirror lock, commands automatically remove stale regular files
whose names start with `.snatchrs-` in the root, metadata, Simple index, and report
directories. Each project's package and Simple directories are cleaned when that
project is processed, without walking the entire mirror. Symlinks, directories,
and `.snatchrs.lock` are kept. Cleanup is best-effort; failures produce warnings.
The removed count is recorded as `stale_temp_files_removed` in check/mirror
inspection and verify reports, and in the `simple` summary.

Workers reserve space for active transfers, including replacements, while always
leaving `--min-free SIZE` free (default `1G`). Sizes accept plain bytes or binary
`K`, `M`, `G`, `T` suffixes, case-insensitively, optionally followed by `B` or `iB`;
for example `512M`, `1GiB`, or `2t`. `--min-free 0` disables the free-space margin.
Pending jobs do not reserve future artifact sizes. A worker waits when another
active reservation is preventing its file from fitting, then rechecks free space
when that reservation is released.

If a file cannot fit alone, or a write reports an exhausted filesystem/quota, the
run stops discovery and stops taking new jobs. Transfers already in progress may
finish. The triggering file is recorded as a failure; queued files that never
started are counted as `not_attempted`, rather than generating thousands of failed
outcomes. The stop message identifies the path, required bytes, available bytes,
and `--min-free`, along with unattempted files and uninspected projects. Free space
and rerun: completed distributions are kept and partial files are cleaned up.

A conflicting package, invalid download path, or package size overflow is recorded
in the package-error journal and discovery continues. A closed/broken queue or
spool-write failure stops the whole producer and is recorded once as a queue-wide
error. Active downloads finish; untouched queued jobs are not attempted. The final
report is still saved on handled stop/error paths. Report persistence itself
requires a writable mirror filesystem. A filesystem lock prevents simultaneous
`check`, `mirror`, and `simple` commands from using the same root. Existing files
are checked by size only; use `snatchrs verify` to revalidate their recorded hashes.

In one terminal display, progress includes packages checked and the current name,
metadata errors, unavailable projects, files discovered, queued/active downloads, byte counts, transfer
speed, and a progress line for each worker with package/filename/attempt. Download
totals grow during discovery, so the UI labels them as provisional until checking
finishes. Workers and live progress remain responsive during slow metadata requests.
For noninteractive runs, concise progress is logged to stderr every five seconds.
`--no-progress`, `--quiet`, and `--json` work as for the other commands.

Saved files include:

- `check-report.json`: `selection`, seed, inspection counters, `error_count`, and paths to detail files; written when discovery ends. `selection` is `all`, `packages`, `packages_file`, or `sample`. `selected_packages` contains normalized names in selection order for the latter three modes and is absent for `all`; `packages_file` records the supplied input path only for that mode.
- `mirror-progress.json`: periodically updated checking, worker, and transfer progress. `not_attempted` counts queued files never started; `stopped` holds the run-wide stop reason or null.
- `mirror-report.json`: inspection summary, download counters, `downloads.outcomes_path`, and final size checks. The
  `simple_index` object records `projects`, `files_linked`, `files_skipped`, and
  `projects_removed`. The first three describe pages generated during the run;
  `projects_removed` counts obsolete pages pruned in the final pass of a `--all`
  mirror run, including runs with per-project failures. `page_errors` counts all
  page-generation failures; `errors` lists the first 100 `{name, error}` records.
  A failed project page does not prevent other pages, the root index, or the final
  report from being written. `downloads.min_free` records the configured margin
  in bytes; `downloads.progress` includes `not_attempted` and `stopped`.
The inspection summary also records `checked` (successful local inspections),
`error_count` (package failures), `unavailable_count`, `not_inspected`, and
`queue_error` (a run-wide reason or null). The first four counters partition the
selection. Queue-wide errors appear once in the error journal with name
`<queue>` and do not inflate the package-error count.

- `reports/run-*/checked-packages.jsonl`: one compact inspection per project: name, last serial, mirror-relative `metadata_path`, `release_count`, file-state counts, and `problems` containing only non-matching files. Healthy files have no per-file run records; their URLs, hashes, sizes, and other metadata stay in `metadata/<name>.json`. `problems` is omitted when empty (`inspection.packages_path`).
- `reports/run-*/package-errors.jsonl`: one metadata/queue error per line, created only on the first error. `inspection.errors_path` is absent when no error journal was created.
- `reports/run-*/unavailable-projects.jsonl`: one project metadata 404 per line, with name, requested metadata URL, `status: "not_found"`, and `http_status: 404`. Created only on the first 404; `inspection.unavailable_path` is absent otherwise.
- `reports/run-*/download-outcomes.jsonl`: one flat completed outcome per line, including failures: `package`, `filename`, `local_path`, `size`, `attempts`, `downloaded`, `sha256_verified`, and `error`. URLs and hashes are not copied into the journal.

Both `check` and `mirror` append details as work completes, rather than retaining
all package metadata or completed download outcomes in memory. Final size checks
read one project's metadata cache at a time through its journal reference. `--json` prints the compact summary, using the
same format as the saved report. The previous inline `packages`, `errors`, and
`downloads.outcomes` arrays have been replaced by the JSONL paths above; consumers
should stream those files line by line. Selected names are retained for explicit lists/samples, and `--all` retains root-index
identities for authoritative pruning. Download deduplication keeps only the
current package's paths and fingerprints. Metadata held by active workers and
the bounded result buffer must also fit in memory.

Each run has its own detail directory with compact counts, problem files, and
completed download outcomes. It references the shared `metadata/` cache rather
than copying it; metadata describes the latest inspection, not a historical
snapshot for each run. New directories use UTC-sortable names such as
`run-20261026T120000Z-<random>` and `verify-20261026T120000Z-<random>`.
After locking, `check`, `mirror`, and `verify` keep the newest 10 directories in
each group by default, then create the new run. Thus up to 11 ordinary directories
per group remain. `--keep-reports N` changes that limit; `--keep-reports 0`
disables pruning. Legacy names sort by modification time before timestamped runs.
Directories referenced by the current check/mirror/verify reports are always
preserved, as are symlinks and directories containing non-regular entries; those
exceptions can leave more than N+1 directories. Unparseable current reports
disable pruning for their group. Pruning is best-effort and counted in
`reports_pruned` alongside `stale_temp_files_removed`. Older unreferenced journal
paths may disappear after later runs. JSON files and JSONL journals
are readable by other users (0644), and run/verify directories are traversable
(0755), so a separate nginx user can read them. JSONL files may end
with an incomplete line if the process is interrupted; only complete lines should
be consumed from an interrupted run.

Metadata errors, failed downloads, missing/wrong-sized files, blocked paths,
queue stops, and page-generation failures produce a nonzero exit code after the
final report is saved. Reports use atomic
replacement. Mirror runs also publish a static HTML Simple Index usable by pip
and uv; see [Serving the mirror](#serving-the-mirror).

For a 10,000-package download run:

```bash
./target/release/snatchrs mirror ./mirror --sample 10000 --seed 42 --concurrency 8
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
python3 scripts/verify-mirror.py ./mirror
```

For a long-running mirror, run the command inside your terminal multiplexer.
The optional runner accepts a mirror root followed by any `mirror` options:

```bash
SNATCHRS_BIN=./target/release/snatchrs bash scripts/run-mirror.sh ./mirror --sample 10000 --concurrency 8
```

The runner uses `snatchrs` from `PATH` unless `SNATCHRS_BIN` is provided. It records
`run.status` under the chosen mirror root and does not provision or format disks.
The verification script writes `verification.json`. It reports metadata errors
separately and returns nonzero for artifact integrity errors or download failures.

## Retry policy

`--attempts` applies to index requests, per-project metadata requests, and file
downloads. It defaults to 3 total attempts, including the initial request, and
accepts 1–10. Use `--attempts 1` to disable retries.

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
./target/release/snatchrs check ./mirror --package packaging --attempts 5
./target/release/snatchrs mirror ./mirror --sample 100 --attempts 5
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

./target/release/snatchrs mirror ./mirror --sample 100 --concurrency 8
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

The `snatchrs` library target is internal (shared by the binary and tests); it has no stable API and is not published.

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

## Verifying a mirror

```sh
snatchrs verify ./mirror
snatchrs verify ./mirror --package packaging -j 4
```

Verification works offline, takes the mirror lock, and streams each file's SHA256
against saved metadata. It checks sizes and rejects symlinks and unsafe paths.
Files without a recorded hash are counted as `no_hash` and do not fail verification.
Missing files, size or hash mismatches, blocked paths, read errors, and invalid
project metadata produce a nonzero exit status; otherwise the exit status is zero.

To repair same-size corrupt files, remove them and rerun mirror with the same
selection:

```sh
snatchrs verify ./mirror --package packaging --remove-bad
snatchrs mirror ./mirror --package packaging
snatchrs verify ./mirror --package packaging
```

`--remove-bad` deletes only files with matching sizes and incorrect SHA256 values,
updates affected Simple pages, and still exits nonzero to report the corruption.
For a whole mirror, omit `--package` when verifying and use `mirror ./mirror --all`
when repairing. Other problems require inspection; size mismatches and missing
files can also be repaired by `mirror`.

The atomic `verify-report.json` contains counts, bytes hashed, removals, and a
mirror-relative `problems_path` pointing to `reports/verify-*/problems.jsonl`.
That journal records each problem file and project metadata error. `--json`
prints the report instead of the plain summary. Progress shows files checked and
bytes hashed; `--no-progress` uses plain status lines and `--quiet` suppresses them.

## Serving the mirror

`snatchrs mirror` writes static HTML [Simple API](https://packaging.python.org/en/latest/specifications/simple-repository-api/)
pages after downloads finish. The mirror root contains:

```text
simple/index.html                        # sorted links to all existing project pages
simple/<normalized-name>/index.html       # links to available local distributions
packages/<normalized-name>/<filename>     # existing artifact layout
```

Project pages link only distributions whose local files currently have the
expected size. Missing, wrong-sized, symlinked, and other nonregular files are not
linked. Available links include the upstream SHA256 when known, `requires-python`,
and yanked status. Filenames are percent-encoded; all HTML attributes and text
are escaped. Pages are written atomically and use mode 0644 on Unix. Files are
not rehashed during page generation: size checks retain the existing mirror's
verification policy. Only HTML is served; JSON Simple API responses and content
negotiation are not implemented.

To generate pages for an existing mirror, or update links after manually removing
artifacts, run:

```bash
./target/release/snatchrs simple /srv/pypi
```

This takes the mirror lock, reads one `metadata/*.json` file at a time, and prints
counts of projects written, files linked, files skipped as not present, and
obsolete project pages removed. It
uses local metadata without network access. Root-index generation scans existing
`simple/<name>/index.html` pages, so earlier sampled projects remain discoverable.
A metadata 404 during `mirror` removes that project's page and root-index entry,
while retaining its local artifacts. `check` does not create, update, or remove
pages. Standalone `simple` treats `metadata/` as authoritative: if a project's
metadata file is absent (including after a `check` sees a 404), its obsolete page
is removed, while its distributions under `packages/` remain untouched. A file
that exists but is corrupt or unreadable keeps its old page. Such metadata/page
errors are listed and skipped while other projects and the root index are rebuilt;
the command then exits nonzero. Error details printed by `simple` and stored under
`simple_index.errors` are capped at 100, while the error count stays exact.

A minimal [nginx](https://nginx.org/en/docs/http/ngx_http_core_module.html#try_files)
server block exposes only the two public trees:

```nginx
server {
    listen 80;
    server_name mirror.example.org;
    root /srv/pypi;
    index index.html;

    location /simple/ {
        try_files $uri $uri/ =404;
    }
    location /packages/ {
        try_files $uri =404;
    }
    location / {
        return 404;
    }
}
```

Point `root` at the complete mirror root and give the nginx worker read access to
public files and traversal access to their parent directories. Keep `metadata/`,
`reports/`, lock files, and root-level JSON reports private: the fallback location
above refuses them. The Python static server used in tests serves the whole root
and is only a loopback test fixture, not this production access policy.

For pip, replace its index with the mirror's `/simple/` URL. For plain HTTP,
explicitly trust that hostname:

```bash
python3 -m pip install --index-url http://mirror.example.org/simple/ --trusted-host mirror.example.org packaging
```

The equivalent Linux user configuration in `~/.config/pip/pip.conf` is:

```ini
[global]
index-url = http://mirror.example.org/simple/
trusted-host = mirror.example.org
```

For HTTPS with a valid certificate, use an `https://` index URL and omit
`trusted-host`. See pip's [index option](https://pip.pypa.io/en/stable/cli/pip_install/#cmdoption-index-url)
and [configuration reference](https://pip.pypa.io/en/stable/topics/configuration/).
The mirror must contain the requested package and its dependencies; a sampled
mirror may not contain every dependency needed for an install.

With uv, select the same index directly or through its environment variable:

```bash
uv pip install --index-url http://mirror.example.org/simple/ packaging
UV_INDEX_URL=http://mirror.example.org/simple/ uv pip install packaging
```

See uv's [index options](https://docs.astral.sh/uv/reference/cli/#uv-pip-install--index-url).

The end-to-end pip smoke test builds a valid wheel using only Python's standard
library, mirrors it from a loopback fixture, stops the upstream fixture, and uses
real pip to download the wheel through a second static HTTP server. It checks a
`+local` filename and the downloaded bytes/hash:

```bash
python3 scripts/pip-smoke.py --binary ./target/release/snatchrs
# If the system Python does not have pip:
uv venv --seed /tmp/snatchrs-pip-test
python3 scripts/pip-smoke.py --binary ./target/release/snatchrs --pip '/tmp/snatchrs-pip-test/bin/python -m pip'
```

The script itself has no third-party Python dependencies. `--pip` accepts a pip
executable or command and defaults to `python3 -m pip`; `--temp-root` optionally
controls the location of its automatically cleaned fixture directories.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this project by you, as defined in the Apache-2.0 license, shall
be dual licensed as above, without any additional terms or conditions.
