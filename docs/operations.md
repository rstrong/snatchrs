# Operations

Run the same selection again to update a mirror and repair missing files.

## Concurrency and progress

| Control | Commands | Default | Range |
| --- | --- | --- | --- |
| `-j`, `--concurrency` | `mirror` downloads | 8 | 1–64 |
| `--metadata-concurrency` | `check`, `mirror` | 4 | 1–32 |
| `-j`, `--concurrency` | `verify` hashing | 4 | 1–64 |

Metadata inspection and downloads overlap. Each inspected project immediately
queues missing or wrong-sized files; checking can continue while download workers
are busy. Totals grow during discovery and are provisional until it finishes.
Reduce metadata concurrency if individual responses consume too much memory.

Interactive terminals show project, queue, byte, and worker progress. Redirected
stdout, `--json`, or `--no-progress` selects plain output; noninteractive long
checks/downloads log status about every five seconds. `--quiet` suppresses status,
not results or failures. `index --no-progress` prints names instead of a dashboard.
See [how it works](how-it-works.md) for the pipeline.

## Disk space

```sh
snatchrs mirror ./mirror --package packaging --min-free 512M
```

`--min-free SIZE` applies only to `mirror` and defaults to `1G`. Accept plain bytes
or binary `K`, `M`, `G`, `T` suffixes, case-insensitively, optionally followed by
`B` or `iB`: `512M`, `1GiB`, and `2t` are valid. `--min-free 0` disables the margin,
not the check that an individual transfer can fit.

Workers reserve space for active transfers, including replacements. Pending jobs
do not reserve all their future sizes. A worker can wait for another reservation
to finish, then recheck free space. Reserve room for metadata and reports too.

If one file cannot fit alone, or a write reports disk/quota exhaustion, discovery
and new downloads stop. In-flight downloads may finish. The triggering file is a
failure; untouched queued jobs count as `not_attempted`. The message gives the
path, required and available bytes, free-space margin, and uninspected projects.
Free space and rerun; completed files remain and partial files are cleaned up.

## Interrupts and locking

For `mirror`, `check`, `simple`, and `verify`, the first Ctrl-C, SIGTERM, or SIGHUP
requests a graceful stop. Downloads are cancelled, workers stop taking new work,
and hashing stops. A metadata request already in flight may finish. The terminal
is restored and reports are saved where the command writes them. Exit status is
130. Rerun the same command to recheck files and continue.

A second signal exits immediately with status 130. `index` retains default
immediate signal handling and does not save a report. Completed artifacts are
kept. Partial transfers restart from zero; this is not byte-range resume.

A filesystem lock prevents simultaneous `check`, `mirror`, `simple`, or `verify`
commands from using the same mirror root. On lock contention, the command fails.
Do not remove a lock file to bypass a running process.

### Stale temporary files

After locking, commands sweep the mirror root and the top level of `metadata/`,
`simple/`, and `reports/`. A project's package and Simple directories are swept
when it is processed; there is no full startup walk of all package directories.
Only regular files starting with `.snatchrs-` are removed. Links, directories,
and `.snatchrs.lock` are kept. Errors are warnings, not run failures.

`stale_temp_files_removed` records the count in check/mirror inspection and
verify reports, and in `simple` output. Report-directory pruning is separate;
see [retention](reports.md#retention).

## Failures and exit codes

| Exit | Meaning |
| --- | --- |
| 0 | Success, including `check` with missing or wrong-sized files only. |
| 1 | A handled failure; inspect stderr and any saved reports. |
| 2 | CLI usage error, such as a missing selection or directory. |
| 130 | Gracefully interrupted command, or forced second-signal exit. |

`check` fails for project errors or blocked paths, but ordinary missing and
size-mismatched files do not fail it. `mirror` fails for inspection, queue,
download, page, or final-file problems. Project metadata 404s are separate
[unavailable outcomes](selecting-packages.md#unavailable-projects), not failures.
`verify` fails for integrity/read/path problems or project metadata errors;
`no_hash` alone succeeds. `simple` fails for metadata/page errors.

A conflicting package, invalid task path, or package-size overflow is recorded as
a package error while other projects continue. A closed/broken queue or spool
write failure stops discovery and is recorded once as a queue-wide error.
Active downloads can finish; untouched jobs are not attempted. Counts distinguish
package failures, unavailable projects, and uninspected projects.

Failed individual pages do not stop other pages, the root page, or the final
report. Error lists for page generation are capped at 100, while counts remain
exact. Handled stops attempt to save reports, but startup/network failures can
precede report creation, and report persistence requires a writable filesystem.
A broken stdout pipe is treated as success rather than a CLI failure.

## Retries and timeouts

`--attempts N` is global: 3 total attempts by default, range 1–10, including the
first attempt. Set 1 to disable retries for index, project metadata, and artifacts.

```sh
snatchrs check ./mirror --package packaging --attempts 5
```

Retryable failures include connection/read timeouts, interrupted connections and
truncated bodies, and HTTP 408, 429, 500, 502, 503, and 504. Other HTTP errors,
malformed metadata, invalid paths, filesystem errors, space failures, and size or
SHA256 mismatches are not retried. An exhausted project fetch is recorded once,
and checking continues with the remaining selection.

Backoff starts at 250–500 ms with jitter and doubles up to a 30-second client cap.
A valid [Retry-After](https://www.rfc-editor.org/rfc/rfc9110.html#name-retry-after)
is a minimum wait, in seconds or as an HTTP date, with jitter added after it.
Server delays are not capped; invalid headers use ordinary backoff, and past
dates add no server delay. Interrupts wake retry and reservation waits.

| Timeout | Limit |
| --- | --- |
| Connection | 15 seconds |
| Index/project metadata read stall | 60 seconds without response data |
| Artifact read stall | 900 seconds without response data |

There is no total transfer deadline. Metadata/API responses should be prompt;
artifact mirrors can be slower. Artifact retries remove the failed temporary
file, release its reservation, and restart from zero. Previously published files
remain intact. Worker progress identifies retry waits and their delay.

## Proxies

Set environment variables before starting snatchrs. They apply to discovery,
metadata, and every artifact download.

| Variable | Lowercase equivalent | Purpose |
| --- | --- | --- |
| `HTTP_PROXY` | `http_proxy` | Proxy for HTTP destinations. |
| `HTTPS_PROXY` | `https_proxy` | Proxy for HTTPS destinations, using CONNECT. |
| `ALL_PROXY` | `all_proxy` | Fallback if no protocol-specific proxy is set. |
| `NO_PROXY` | `no_proxy` | Comma-separated hosts/addresses reached directly. |

Replace the proxy hostname with your proxy:

```sh
export HTTP_PROXY=http://proxy.example.org:3128
export HTTPS_PROXY=http://proxy.example.org:3128
export NO_PROXY=localhost,127.0.0.1
snatchrs mirror ./mirror --package packaging
```

An `http://` proxy URL in `HTTPS_PROXY` is normal: CONNECT tunnels the TLS
connection, and destination certificates are still checked. Proxy URLs may
include credentials. Protocol-specific variables override `ALL_PROXY`;
`NO_PROXY` bypasses matching destinations. Proxy failures remain request/download
errors; there is no fallback around a failed proxy. Transport follows
[reqwest's environment proxy behavior](https://docs.rs/reqwest/0.12.28/reqwest/#proxies).
See [proxy tests](development.md#checks) for isolated local regression tests.

## Verify and repair

```sh
snatchrs verify ./mirror
snatchrs verify ./mirror --package packaging -j 4
```

Verification is offline and takes the mirror lock. It needs `metadata/` and the
local artifacts, not previous reports. It creates a fresh verify report and
problem journal, so the mirror directory must be writable.

`verify` checks every project in `metadata/`. `check` saves metadata too, so a
project you only checked, and never mirrored, shows all its files as `missing`.
Mirror it, or pass `--package` for the projects you actually mirrored.

| State | Meaning | Fails? |
| --- | --- | --- |
| `ok` | Size and recorded SHA256 match. | No |
| `no_hash` | Size matches, no SHA256 recorded. | No |
| `missing` | File absent. | Yes |
| `size_mismatch` | Expected and actual sizes differ. | Yes |
| `hash_mismatch` | Same size, incorrect SHA256. | Yes |
| `blocked` | Unsafe path, symlink, or nonregular file. | Yes |
| `error` | Read failure while hashing. | Yes |

Unreadable or corrupt metadata is a project error, and other projects continue.
Hashes compare case-insensitively. To remove same-size corrupt files and repair:

```sh
snatchrs verify ./mirror --package packaging --remove-bad
snatchrs mirror ./mirror --package packaging
snatchrs verify ./mirror --package packaging
```

The first command still exits nonzero and retains `hash_mismatch` in its report,
plus `removed`. It deletes only matching-size files with an incorrect hash and
updates affected project and root pages. It preserves symlink targets,
wrong-sized files, and other artifacts. Ordinary stale-temp cleanup still runs.
For a whole-mirror repair, omit `--package` when verifying and use `snatchrs mirror ./mirror --all`.
Missing and wrong-sized files can be repaired by `mirror` without `--remove-bad`.
Inspect other errors yourself. See [verify reports](reports.md#verification).

## Long runs

Run long operations in tmux or another terminal multiplexer. For example, from
the clone directory:

```sh
tmux new -s pypi-mirror
bash scripts/run-mirror.sh ./mirror --packages-file requirements.txt
```

The runner uses `snatchrs` on PATH unless `SNATCHRS_BIN` overrides it. It writes
`running` and then `exit=N` to `run.status`, prints the report path, and returns
the command's exit status. It does not provision or format disks.

For cron, replace all `/absolute/path/...` placeholders and supply the full PATH
needed by your installed binary. Example crontab entry, daily at 02:00:

```cron
0 2 * * * cd /absolute/path/to/snatchrs && PATH=/absolute/path/to/cargo/bin:/usr/bin:/bin bash scripts/run-mirror.sh /absolute/path/to/mirror --packages-file /absolute/path/to/requirements.txt >> /absolute/path/to/mirror.log 2>&1
```

A concurrent invocation fails on the mirror lock instead of running two writers.

## List the upstream index

```sh
snatchrs index
snatchrs index --json > index.json
snatchrs index --index-url https://test.pypi.org/simple/
snatchrs index --from-file tests/fixtures/index.json --no-progress
```

A live `index` fetches the complete root list once; expect a large response. It
does not fetch project metadata or artifacts, nor persist results unless stdout
is redirected. `--from-file` expects raw Simple Index API JSON, not snatchrs'
enriched JSON output. `--index-url` supplies project URLs when reading a fixture.
The fixture example assumes the working directory is the source clone.

[Back to README](../README.md)
