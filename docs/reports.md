# Reports and layout

Use the latest root-level summary to find that run's detail journals.

## Mirror directory

```text
mirror/
  .snatchrs.lock
  metadata/<normalized-name>.json
  packages/<normalized-name>/<distribution-filename>
  simple/index.html
  simple/<normalized-name>/index.html
  check-report.json
  mirror-progress.json
  mirror-report.json
  verify-report.json
  reports/
    run-YYYYMMDDTHHMMSSZ-<random>/
      checked-packages.jsonl
      package-errors.jsonl             # only when needed
      unavailable-projects.jsonl       # only when needed
      download-outcomes.jsonl          # mirror only
    verify-YYYYMMDDTHHMMSSZ-<random>/
      problems.jsonl
  run.status                           # optional runner
  verification.json                    # independent Python verifier
```

`check` creates metadata, project directories, and inspection reports without
fetching artifacts. `mirror` additionally downloads files and publishes pages.
Only `simple/` and `packages/` need to be served. Preserve `metadata/` for offline
verification and page rebuilds. Old reports are not inputs to `verify`.

All stored filesystem paths are mirror-relative. Resolve them against the
positional mirror directory, not the working directory or the JSON file's own
parent. Examples are `metadata/packaging.json` and
`packages/packaging/packaging-25.0.tar.gz`. Reports use `mirror_root: "."`.
Download URLs remain absolute HTTP/HTTPS URLs. Copying the whole directory keeps
those paths usable on another host.

## Saved metadata

`metadata/<name>.json` contains the project identity, metadata URL and source,
metadata path, package directory, last serial, and releases. Each release lists
its version label and all distribution files, including wheels, sources, and
yanked files. Empty advertised releases remain. Version labels sort lexically,
not by Python version precedence.

A file records its filename, download URL, relative local path, expected bytes,
optional SHA256, distribution type, Python requirement, and yanked state. These
details are shared by later runs; run journals do not copy them. See
[association rules](how-it-works.md#release-labels) and
[cache reuse](selecting-packages.md#metadata-cache).

## Inspection summary

`check-report.json` is written when inspection ends. The `inspection` object in
`mirror-report.json` uses the same fields:

| Field | Meaning |
| --- | --- |
| `mirror_root` | `.`; resolve paths against the command's mirror directory. |
| `selection` | `all`, `packages`, `packages_file`, or `sample`. |
| `seed` | Sampling seed, or null. |
| `selected_packages` | Normalized names in selection order; absent for `all`. |
| `packages_file` | Supplied selection-file path; only for that mode. |
| `metadata_concurrency` | Configured metadata worker limit. |
| `summary` | Counts and expected bytes described below. |
| `checked` | Successful local inspections. |
| `error_count` | Project failures. |
| `unavailable_count` | Project metadata 404s. |
| `not_inspected` | Projects left uninspected after a stop. |
| `queue_error` | Queue-wide stop reason, or null. |
| `stopped` | `Interrupted` when inspection was interrupted, otherwise null. |
| `packages_path` | Compact checked-project JSONL path. |
| `errors_path` | Error journal path; absent if none was created. |
| `unavailable_path` | 404 journal path; absent if none was created. |
| `stale_temp_files_removed` | Stale temporary files removed. |
| `reports_pruned` | Old report directories removed before this run. |

`checked`, `error_count`, `unavailable_count`, and `not_inspected` partition the
selection. A queue-wide error appears once with name `<queue>` and does not
inflate the package-error count.

`summary` contains `packages`, `releases`, `files`, `expected_bytes`, `missing`,
`size_mismatch`, `matching_size`, `blocked`, and `stale_temp_files_removed`.
`expected_bytes` totals all selected files, including existing files; it is not
the remaining download size. `blocked` here includes nonregular files and
inspection errors. Plain `check` prints counts; use `--json` for expected bytes.

## Inspection journals

`checked-packages.jsonl` has one line per inspected project. Fields are `name`,
`last_serial`, `metadata_path`, `release_count`, `files`, `missing`,
`size_mismatch`, `matching_size`, `blocked`, and `errors`. Its `problems` array
contains only files not matching size and is omitted when empty.

Each problem records `filename`, `version`, `local_path`, and `status`, plus
`actual_size` for a mismatch or `message` for an inspection error. Statuses are
`missing`, `size_mismatch`, `not_regular_file`, or `error`. Here `blocked` counts
nonregular files and `errors` counts file-inspection errors separately.
Healthy files have no per-file journal entry: find their details in `metadata/`.
There are no copied URLs, hashes, or release arrays in checked-project lines.

`package-errors.jsonl` records `{name, error}`. `unavailable-projects.jsonl`
records `name`, requested `metadata_url`, `status: "not_found"`, and
`http_status: 404`. Each file is created only when its first record is written;
its report path is absent otherwise.

Details follow completion order, which may differ from selection order with
parallel metadata workers. Stream JSONL one complete line at a time.

## Download progress and outcomes

`mirror-progress.json` is updated periodically. It records discovery, total files
and bytes, downloaded/failed/not-attempted counts, verified and transferred bytes,
elapsed seconds, a stopped reason or null, and per-worker state. Discovery
includes checked/total projects, current name, release/file/problem counts,
unavailable projects, errors, and whether discovery finished. Worker state
includes project, filename, bytes, size, attempt, state, and retry delay.
Totals are provisional until discovery finishes.

`mirror-report.json` contains `inspection`, `downloads`, `final_check`, and
`simple_index`. `downloads` records concurrency, the free-space margin in bytes,
final progress, and `outcomes_path`. Its JSONL lines are flat records with:

```text
package, filename, local_path, size, attempts,
downloaded, sha256_verified, error
```

The outcome journal includes failures, but no copied `download_url` or `sha256`.
`not_attempted` counts queued jobs abandoned by a stop, including transfers
cancelled on interruption; they have no completed outcome line.

`final_check` repeats local size inspection after downloads. `simple_index`
records `projects`, `files_linked`, `files_skipped`, `projects_removed`,
`page_errors`, `errors`, and `stale_temp_files_removed`. The first three describe
pages generated in this run. `projects_removed` counts obsolete pages pruned by
`--all`; `errors` holds the first 100 `{name, error}` details while the error count
remains exact. Other pages and the root are still attempted after page errors.

## Verification

`verify-report.json` is written atomically. It contains `projects`, `files`,
`bytes_hashed`, `ok`, `no_hash`, `missing`, `size_mismatch`, `hash_mismatch`,
`blocked`, `error`, `project_errors`, `removed`, `stopped`,
`stale_temp_files_removed`, `reports_pruned`, and `problems_path`.

`reports/verify-*/problems.jsonl` records each problem file with `project`,
`filename`, `local_path`, `state`, and `detail`. A project metadata failure uses
`state: "project_error"`. Healthy and `no_hash` files are not listed. Interrupted
hashing preserves partial byte and completed-file counts. A file removed by
`--remove-bad` still counts as `hash_mismatch`, and the command still fails;
`removed` counts deletions. See [repair](operations.md#verify-and-repair).

## Retention

New directories use a UTC timestamp and a random suffix. After locking and
before creating a new run, `check`, `mirror`, and `verify` prune the two groups
independently. The default keeps the newest 10 in each group, then adds the new
run, leaving up to 11 ordinary directories. Use `--keep-reports N` to change the
limit or `--keep-reports 0` to disable pruning. Names without a timestamp sort
before timestamped names, by modification time.

Directories referenced by the current check, mirror, or verify report are always
kept. Only real directories directly under `reports/`, matching `run-*` or
`verify-*`, and containing only regular files can be deleted. Symlinked entries,
subdirectories, special files, and unrelated names are untouched. An unparseable
current report disables pruning for its group. Deletion failures are warnings
and do not fail the run. Protected or unsafe entries can leave more than N+1.

`reports_pruned` records the removed-directory count. Earlier unreferenced paths
may disappear after later runs. Even retained journals reference the latest
metadata cache; they are not self-contained historical snapshots.

## Permissions and interrupted writes

JSON reports and metadata are published atomically through temporary files.
On Unix, JSON and JSONL files use 0644; run/verify directories use 0755. Other
parents follow the umask; see [serving permissions](serving.md#permissions).

Journals are appended as work completes. A forced interruption can leave an
incomplete last line; consume only complete lines. Reports still need writable
storage to be saved. Keep completed and interrupted details until retention
removes them or you no longer need them.

`--json` on `check`, `mirror`, or `verify` prints the same summary format saved
at the mirror root, without an interactive dashboard. `index --json` instead
prints the discovered index, not a run report. `simple` has no `--json` option.

[Back to README](../README.md)
