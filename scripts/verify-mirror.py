#!/usr/bin/env python3
"""Independently verify the sizes and SHA256 hashes in a completed mirror report."""
import argparse
import hashlib
import json
from pathlib import Path
import sys


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("mirror", type=Path)
    args = parser.parse_args()
    root = args.mirror.resolve()
    report = json.loads((root / "mirror-report.json").read_text())
    checked = {}
    failures = []
    verified_bytes = 0
    hashed = 0
    metadata_packages = 0
    with Path(report["inspection"]["packages_path"]).open() as journal:
        packages = map(json.loads, journal)
        for package in packages:
            metadata_packages += 1
            for release in package["metadata"]["releases"]:
                for file in release["files"]:
                    path = Path(file["local_path"])
                    if path in checked:
                        continue
                    checked[path] = True
                    try:
                        if not path.resolve().is_relative_to(root) or path.is_symlink() or not path.is_file():
                            raise ValueError("not a regular file within the mirror")
                        if path.stat().st_size != file["size"]:
                            raise ValueError("size mismatch")
                        if file["sha256"]:
                            with path.open("rb") as source:
                                actual = hashlib.file_digest(source, "sha256").hexdigest()
                            if actual != file["sha256"].lower():
                                raise ValueError("SHA256 mismatch")
                            hashed += 1
                        verified_bytes += file["size"]
                    except (OSError, ValueError) as error:
                        failures.append({"path": str(path), "error": str(error)})
    result = {
        "selected_names": len(report["inspection"]["selected_packages"]),
        "metadata_packages": metadata_packages,
        "unavailable_projects": report["inspection"].get("unavailable_count", 0),
        "metadata_errors": [json.loads(line) for line in Path(report["inspection"]["errors_path"]).read_text().splitlines()],
        "checked_files": len(checked), "sha256_verified_files": hashed,
        "verified_bytes": verified_bytes, "failures": failures,
        "download_failures": report["downloads"]["progress"]["failed"],
    }
    (root / "verification.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result, indent=2))
    return 1 if failures or result["download_failures"] else 0


if __name__ == "__main__":
    sys.exit(main())
