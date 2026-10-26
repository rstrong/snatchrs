# How it works

snatchrs inspects files locally, downloads missing data, and publishes a static index.

## Upstream API and names

Discovery requests the [JSON Simple Index API](https://docs.pypi.org/api/index-api/)
with `Accept: application/vnd.pypi.simple.v1+json`. Project metadata comes from
the per-project Simple endpoint, not the deprecated project JSON API. snatchrs
accepts API major version 1 and ignores unknown fields. Malformed names or
responses fail explicitly; they are not silently skipped.

Names follow
[PyPA normalization](https://packaging.python.org/en/latest/specifications/name-normalization/):
lowercase, with runs of dots, underscores, or hyphens replaced by one hyphen.
Saved metadata keeps the original and normalized names, project URL, and optional
PyPI last-change serial. The root index also has an API version and optional
repository serial. [Selection and cache reuse](selecting-packages.md) explain
when the root listing or individual metadata is fetched.

The project's flat `files` list supplies URLs, sizes, hashes, Python requirements,
and boolean or string yanked values. Relative URLs resolve against the final
response URL, including redirects. Sizes are required because local inspection
relies on them. Unsafe filenames and symlinked package directories are rejected.

## Release labels

Files are grouped by advertised versions using filenames, PEP 440 equivalence,
and legacy egg/installer patterns. Distribution type is inferred from the name.
Equivalent advertised spellings are resolved deterministically. Legacy names
prefer the longest matching advertised version prefix.

A release label is informational; it does not decide whether to mirror a file.
If no advertised release fits, a filename-derived label or `unknown` keeps the
file in metadata and the download flow. A single advertised version also serves
as a fallback. When an older Simple response omits `versions`, recognizable
filenames supply labels. Empty advertised releases remain, but an empty version
string in metadata is invalid.

## Inspection and the pipeline

1. Discover the selection, or construct it from explicit project names.
2. A bounded metadata pool fetches or reuses one project's metadata per job.
3. Inspect its local files, append a compact journal line, and enqueue downloads.
4. Download workers consume pending jobs while later projects are still checked.
5. Recheck files, write selected project pages, and rebuild the root index.

A slow metadata request or retry does not block other metadata workers. The
checker can keep queuing jobs even when all download workers are occupied.
Pending jobs are spooled to a temporary JSONL file under the mirror root, read
in queue order. This spool is per-run, not a persistent resume queue. A rerun
inspects the disk again and skips completed files.

Task deduplication is by local path within the current project. Conflicting
metadata for a path fails that project instead of downloading inconsistent data.
Metadata and download concurrency are separate
[operational controls](operations.md#concurrency-and-progress).

## Size checks and hash checks

| Operation | Size | SHA256 |
| --- | --- | --- |
| `check` or existing-file inspection in `mirror` | Compared. | Not rehashed. |
| New download | Must match. | Must match when recorded upstream. |
| `simple` page generation | Compared before linking. | Not rehashed. |
| `verify` | Compared. | Rehashed when recorded; otherwise `no_hash`. |

Inspection records `matching_size`, `missing`, `size_mismatch` with actual bytes,
`not_regular_file`, or `error`. A matching size is not an integrity guarantee.
Inspection itself does not change existing artifacts. Same-size corrupt files
need [verify and repair](operations.md#verify-and-repair).

## Atomic publication

Each transfer writes a unique temporary file on the destination filesystem.
Automatic artifact decompression is disabled, so the mirrored bytes are the
upstream distribution bytes. Expected size and optional SHA256 must match before
an atomic replacement publishes the file. A failed replacement preserves any
previous file. Artifacts are never installed, imported, or extracted by snatchrs.

Failed attempts remove their temporary files; partial downloads restart from
zero. JSON and HTML outputs also use temporary files and atomic publication.
A lock serializes writers to the same mirror. See
[space limits and failure handling](operations.md#disk-space).

## Memory and reports

The root listing and selected identities must fit in memory. All-mode retains
root identities for authoritative pruning; explicit lists and samples retain
selected names. Active metadata workers and their bounded result buffer also
hold project metadata. Large individual projects can therefore still be large;
lower metadata concurrency if needed.

The pipeline does not retain all completed metadata or download outcomes. It
streams compact inspection lines, problem details, and outcomes to journals.
Healthy-file details stay in the shared metadata cache. Deduplication retains
only the current project's paths and fingerprints. The final pass loads one
project's metadata through its journal reference at a time. `simple` and `verify`
also read metadata one project at a time; hashing uses a bounded worker pool.
See the [report schemas](reports.md) for exact output fields.

[Back to README](../README.md)
