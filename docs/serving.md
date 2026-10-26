# Serving a mirror

Serve the two public trees with a static web server; no snatchrs daemon is needed.

## Public layout

```text
simple/index.html
simple/<normalized-name>/index.html
packages/<normalized-name>/<distribution-filename>
```

The root page links existing project pages in sorted order, including projects
from previous selections. Project pages link only regular local files with the
expected size. Missing, wrong-sized, symlinked, and other nonregular files are
excluded. Links carry SHA256 when known, Python requirements, and yanked status.
Filenames are percent-encoded, and text and attributes are HTML-escaped.

The served interface is HTML Simple API only. JSON index responses and content
negotiation are not implemented. Files are not rehashed while pages are built;
use [verify](operations.md#verify-and-repair) for SHA256 validation.

## Rebuild pages offline

```sh
snatchrs simple ./mirror
```

`mirror` writes project pages after downloading and then rebuilds the root page.
Run `simple` after manually removing files or to rebuild an existing mirror.
It takes the mirror lock, reads local metadata one project at a time, and needs
no network. It prints projects written, files linked or skipped, obsolete pages
removed, and stale temporary files removed.

For standalone `simple`, `metadata/` is authoritative. A project whose metadata
file is absent loses its page, while its distributions remain. Existing corrupt
or unreadable metadata preserves the old page and records an error. Other pages
and the root are still rebuilt; the command then exits nonzero. Error details
are capped at 100 while error counts remain exact.

A metadata 404 during `mirror` removes the project's page; `check` only removes
the metadata cache and leaves pages alone. A later `simple` then removes that
obsolete page. Removing a page also removes its now-empty project directory, so
clients get 404 rather than an empty directory's 403. Full-index project pruning
is described in [selection](selecting-packages.md#full-index-pruning).

## nginx

Replace `mirror.example.org` and `/srv/pypi` with your hostname and mirror root.
Install this server block through your usual nginx configuration process:

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

Keep the fallback location. It refuses access to `metadata/`, `reports/`, lock
files, and root-level JSON reports. Point `root` at the complete mirror directory,
not its `simple/` subdirectory.

### Permissions

Generated metadata JSON, reports, JSONL journals, HTML pages, and downloads use
0644 on Unix. Run and verify directories use 0755. Other directories follow the
process's umask, including the mirror root, project directories, and their
parents. Use `umask 022` when creating a mirror for a separate nginx user, and
ensure that user can traverse every parent directory.

For an existing mirror, this example grants directory traversal and public-file
read access. Replace the path before running it:

```sh
find /srv/pypi -type d -exec chmod a+rx {} +
find /srv/pypi/simple /srv/pypi/packages -type f -exec chmod a+r {} +
```

Also check parents above `/srv/pypi`. nginx access restrictions still belong in
the server configuration; file permissions alone do not hide internal reports.
A copied mirror's owner and umask may differ from the original machine's.

## pip

For an example plain-HTTP server, replace the hostname in both arguments:

```sh
pip install --index-url http://mirror.example.org/simple/ --trusted-host mirror.example.org packaging
```

Or set Linux user configuration in `~/.config/pip/pip.conf`:

```ini
[global]
index-url = http://mirror.example.org/simple/
trusted-host = mirror.example.org
```

Use the mirror as the index, rather than relying on an upstream fallback on an
air-gapped network. The mirror must contain the selected package and dependencies.
A random sample is not a dependency-complete set. See the
[pip index option](https://pip.pypa.io/en/stable/cli/pip_install/#cmdoption-index-url)
and [configuration reference](https://pip.pypa.io/en/stable/topics/configuration/).

## uv and HTTPS

Use the same Simple URL. These examples assume an active virtual environment;
replace the hostname:

```sh
uv pip install --index-url http://mirror.example.org/simple/ packaging
UV_INDEX_URL=http://mirror.example.org/simple/ uv pip install packaging
```

For HTTPS, configure TLS at the web server and use an `https://` index URL with
a certificate trusted by the clients. Omit pip's `trusted-host` for a valid
certificate. HTTP proxy CONNECT still verifies the destination's certificate;
see [proxies](operations.md#proxies). uv's
[index options](https://docs.astral.sh/uv/reference/cli/#uv-pip-install--index-url)
describe its client configuration.

## Local testing

```sh
python3 -m http.server 8000 --directory ./mirror
```

This serves the entire root, including metadata and reports. Use it only for a
temporary test; it does not enforce the production access policy above.
[Development smoke tests](development.md#smoke-tests) exercise real pip against
an isolated loopback server with the upstream stopped.

[Back to README](../README.md)
