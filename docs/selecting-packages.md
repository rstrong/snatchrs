# Selecting packages

Choose project names; snatchrs mirrors all their releases and distribution files.

## Four selection modes

`check` and `mirror` require a directory and exactly one of these modes. There is
no default directory, implicit sample, version filter, or dependency resolver.

| Mode | Selection | Fetches root index? |
| --- | --- | --- |
| `--package NAME` | Repeat for explicit names. | No |
| `--packages-file PATH` | Names from a local text file. | No |
| `--sample N` | N random projects, without replacement. | Yes |
| `--all` | Every project, in root-index order. | Yes |

```sh
snatchrs check ./mirror --package requests --package packaging
snatchrs mirror ./mirror --packages-file requirements.txt
snatchrs check ./mirror --sample 10 --seed 42
```

Names are normalized by lowercasing and replacing runs of `.`, `_`, or `-` with
`-`. Duplicates keep their first occurrence. Samples are limited to the index
size; there is no additional package-count cap. Unavailable sampled projects do
not cause replacement names to be drawn.

A sample report records its seed and selected names. A seed reproduces the same
selection only when the root index is unchanged. To repeat exact names after
PyPI changes, use explicit names or a packages file.

## Packages-file syntax

Write one leading project name per line. For example:

```text
# Projects to include
requests>=2.0
Foo[bar]==1; python_version<"3.9"
name @ https://example.org/name.whl
```

Blank lines and comments are ignored. `#` begins a comment only at the start of
a line or after whitespace; URL fragments such as `#sha256=...` stay intact.
Extras, pins, constraints, markers, and direct URLs after the name are ignored too.
The direct URL does not become a download source. All versions of each named
project are selected from the configured index.

This is not a full requirements parser. Invalid names and pip options such as
`-r` and `-e` fail with their line number. An empty or comment-only file is an
error.

Hash-pinned lockfiles work, including output from `uv pip compile
--generate-hashes`, `uv export`, pip-tools and Poetry. Lines ending in `\` are
joined as pip joins them, and `--hash` options are ignored: snatchrs mirrors every
version and checks downloads against the index's own hashes. Other pip options,
such as `-r`, `-e` and `--index-url`, are still errors. An error in a continued
line reports the line number where that requirement starts.

## Include dependencies

Resolve dependencies before giving the names to snatchrs. With uv installed:

```sh
printf 'requests\nflask\n' > requirements.in
uv pip compile requirements.in -o requirements.txt
snatchrs check ./mirror --packages-file requirements.txt --json > check.json
snatchrs mirror ./mirror --packages-file requirements.txt
```

The compiled file names the resolved dependencies. snatchrs ignores its pins
and mirrors all versions of all those projects. Resolution describes your chosen
Python environment; it does not guarantee dependencies for every historical
version or every platform. Include additional platform, optional, and build
dependencies you need, and test the resulting mirror with your actual clients.

Alternatively, run `pip freeze > requirements.txt` in a working environment.
Review that file: editable entries, options, and unnamed URL requirements may
need to be replaced with ordinary project names.

One measured requests + flask set contained 12 projects, 4,736 files, and about
790 MiB. Counts change over time. `check` downloads metadata only; its plain
summary shows counts, while its JSON `summary.expected_bytes` records the total
expected bytes of all selected files, including files already present. That is
not the remaining download size. See [reports](reports.md).

## Full-index pruning

Full PyPI needs many tens of TB. Request it explicitly only after planning
storage and bandwidth:

```sh
snatchrs mirror ./mirror --all
```

A complete root listing is authoritative. In the final pass, `mirror --all`
removes project pages and metadata caches for names absent from that listing.
Per-project metadata or download failures do not prevent this pruning; those
failures still produce a nonzero exit. Names remaining in the listing are kept.
An interrupted run, or one that aborts before the final pass, skips this pruning.

Files under `packages/` are never deleted by project pruning. Other selection
modes never prune unselected projects. `check` never changes public index pages.
Project pruning is separate from [report retention](reports.md#retention).

## Metadata cache

All/sample runs can reuse saved metadata when the root listing's `_last-serial`
matches the cached project serial. Caches are tied to the root-listing project
URL and the requested metadata URL before redirects. Changed or missing serials,
missing or invalid caches, and a changed source trigger a fetch. If JSON omits a
project serial, its `X-PyPI-Last-Serial` header can supply it.

Explicit names and packages files skip the root listing and always fetch project
metadata. Local files are inspected on every run, including cache hits, so missing
and wrong-sized files are still queued for repair. `--refresh` forces a fresh
metadata request for every selected project. A failed refresh keeps the previous
cache but reports an error instead of silently using stale metadata.

```sh
snatchrs mirror ./mirror --sample 100 --refresh
```

## Index sources and metadata workers

`--index-url URL` defaults to `https://pypi.org/simple/`. It supplies both the root
listing and per-project metadata base. `--metadata-url URL` overrides only the
project metadata base; it expects a Simple API base URL, not the deprecated
project JSON endpoint. Each project is requested at `<base>/<normalized-name>/`.

`--metadata-concurrency N` defaults to 4 and accepts 1–32. Results are checked and
queued as they finish. Use 1 for sequential metadata requests, or lower the limit
when projects have unusually large metadata responses. This setting is separate
from the [download worker count](operations.md#concurrency-and-progress).

Details and plain project output follow completion order. `selected_packages`
keeps selection order for explicit lists and samples; `--all` omits that array.

## Unavailable projects

A project metadata HTTP 404 is a `not_found` outcome, not a package error. It is
not retried and does not make the command fail. It increments `unavailable_count`
and invalidates only that project's metadata cache; distribution files remain.
This covers deleted projects still listed upstream and names that never existed.

`mirror` also removes that project's page and later updates the root index.
`check` leaves pages untouched. Other HTTP errors, a root-index 404, and missing
distribution downloads remain errors. See
[failure handling](operations.md#failures-and-exit-codes).

[Back to README](../README.md)
