# Development

Build and test from the source clone with Rust 1.89 or newer.

The `snatchrs` library target is internal (shared by the binary and tests); it has
no stable API and is not published. It can change without notice. Use the CLI as
the supported interface. The CLI uses concise stderr status, pipeable stdout,
subcommands, and quiet/plain/JSON controls.

## Checks

```sh
CARGO_INCREMENTAL=0 cargo build --release --locked
cargo fmt --check
CARGO_INCREMENTAL=0 cargo clippy --all-targets --locked -- -D warnings
CARGO_INCREMENTAL=0 cargo test --locked
CARGO_INCREMENTAL=0 cargo test --locked --test proxy
```

Tests use local HTTP servers and temporary mirrors. They cover index and project
metadata, releases/files, deterministic selection, path safety, local file states,
CLI output, cache reuse, retries, proxy routing, integrity, interruptions,
retention, permissions, and static index generation. File fixtures exercise
matching, missing, wrong-sized, zero-byte, directory, and symlink cases. Proxy
tests isolate environment variables in child processes.

To inspect a small saved index without network traffic:

```sh
./target/release/snatchrs index --from-file tests/fixtures/index.json --no-progress
./target/release/snatchrs --quiet index --from-file tests/fixtures/index.json --json
```

## Smoke tests

```sh
python3 scripts/remote-smoke.py --binary ./target/release/snatchrs
python3 scripts/pipeline-smoke.py --binary ./target/release/snatchrs
python3 scripts/pipeline-smoke.py --binary ./target/release/snatchrs --check-ahead
python3 scripts/pip-smoke.py --binary ./target/release/snatchrs
```

The remote smoke script uses only Python's standard library and loopback HTTP.
It checks missing/wrong-sized files, concurrent downloads, integrity, saved
reports, relocation, repeat-run skipping, and large sample arguments. It also
runs the independent Python verifier and the Rust `verify` command.

The pipeline smoke uses a PTY to render the dashboard and proves downloads start
while later metadata is blocked. `--check-ahead` proves inspection and queueing
can finish while download workers are blocked.

The pip smoke builds a valid `+local` wheel with Python's standard library,
mirrors it from a local fixture, stops that origin, then uses real pip against a
second static server. It verifies the downloaded bytes and hash. The script has
no third-party Python dependencies; it needs a pip command. If system Python
has no pip, use a seeded environment:

```sh
uv venv --seed /tmp/snatchrs-pip-test
python3 scripts/pip-smoke.py --binary ./target/release/snatchrs --pip '/tmp/snatchrs-pip-test/bin/python -m pip'
```

`--pip` accepts an executable or command and defaults to `python3 -m pip`.
`--temp-root` chooses where automatically cleaned fixture directories are made.
Use fresh scratch directories and avoid mirroring large real project sets in
routine tests. Fixtures disable the free-space margin unless testing that limit.

For an independent check of a completed real mirror:

```sh
python3 scripts/verify-mirror.py ./mirror
```

It reads journal metadata references and verifies sizes and SHA256 independently
of Rust. It writes `verification.json`, reports metadata errors separately, and
returns nonzero for artifact integrity failures or recorded download failures.

## Copy a binary to another host

Build a release binary and inspect its target and dynamic-library requirements:

```sh
CARGO_INCREMENTAL=0 cargo build --release --locked
./target/release/snatchrs --version
file target/release/snatchrs
ldd target/release/snatchrs
```

The destination needs no Rust compiler or source tree. It still needs a
compatible operating system, CPU architecture, and system libraries. Do not
assume static linking. On Linux, a binary built against a newer glibc may not run
on an older distribution. Build on a matching host or use an appropriate
cross-compilation toolchain for another OS, architecture, or older glibc.

Replace `user@example.org` with your SSH destination:

```sh
scp target/release/snatchrs user@example.org:~/snatchrs
scp tests/fixtures/index.json user@example.org:~/snatchrs-fixture.json
ssh -t user@example.org './snatchrs index --from-file ~/snatchrs-fixture.json'
```

`ssh -t` allocates a terminal for the dashboard. The saved fixture makes the
remote test work without PyPI access. For live use, install the copied binary as
`snatchrs` on the destination's PATH and follow the [README](../README.md).
For unattended operation, see [long runs](operations.md#long-runs).

## CI and releases

CI runs on pushes to `main`, pull requests, and manual dispatch. Required-check
names are `Lint (stable)`, `Test (ubuntu-latest, stable)`,
`Test (macos-latest, stable)`, and `Build (MSRV)`. Lint checks formatting and
Clippy; tests run on stable Rust on Linux and macOS. The MSRV job reads
`rust-version` from Cargo.toml and builds all targets with that toolchain.
The Linux-only `/dev/full` test is already gated; Unix tests also run on macOS.

To prepare a release, update `version` in Cargo.toml and its matching package
entry in Cargo.lock, then commit both files. Replace `X.Y.Z` below with that
version, review CI, and create and push the matching tag:

```sh
git tag vX.Y.Z
git push origin vX.Y.Z
```

For a release candidate, keep Cargo.toml at the base version and use
`vX.Y.Z-rc.N`, such as `v0.1.0-rc.1`. Other suffixes or mismatched versions fail
the release tag check. Candidates are marked as prereleases and appear on the
releases page rather than GitHub's latest stable release URL.

The tag workflow builds Linux x86_64/ARM64 musl and macOS Apple Silicon/Intel
archives. Linux builds must be static. Native binaries run version and saved-index
smoke tests; the Intel Mac binary also runs if Rosetta is available. Each archive
contains the binary, README, and both licenses in a named top-level directory.
The publish job adds SHA256SUMS, attests the archives' build provenance, and
creates a GitHub release with generated notes. macOS downloads are not Developer
ID signed or notarized; follow the [install instructions](../README.md#install)
for quarantine handling. No crates.io package is published.
