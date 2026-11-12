#!/usr/bin/env python3
"""Download and verify the pinned CEF binary distribution for one platform.

Reads packaging/cef-pin.json, downloads the archive from the official CEF build
service, checks the published SHA-1, and extracts it so that <dest>/Release and
<dest>/include exist.

    python3 packaging/fetch-cef.py --platform linux64 --dest cef
"""

import argparse
import hashlib
import json
import pathlib
import shutil
import sys
import tarfile
import tempfile
import urllib.request

BASE = "https://cef-builds.spotifycdn.com"


def sha1_of(path):
    h = hashlib.sha1()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--platform", required=True)
    ap.add_argument("--dest", required=True)
    ap.add_argument("--pin", default="packaging/cef-pin.json")
    args = ap.parse_args()

    pin = json.loads(pathlib.Path(args.pin).read_text())
    try:
        entry = pin["platforms"][args.platform]
    except KeyError:
        sys.exit(f"platform {args.platform} is not pinned in {args.pin}")

    name = entry["file"]
    url = f"{BASE}/{name}"
    dest = pathlib.Path(args.dest)
    if (dest / "Release").is_dir() and (dest / "include").is_dir():
        print(f"CEF already present at {dest}")
        return

    with tempfile.TemporaryDirectory() as tmp:
        archive = pathlib.Path(tmp) / name
        print(f"downloading {url}")
        urllib.request.urlretrieve(url, archive)
        actual = sha1_of(archive)
        if actual != entry["sha1"]:
            sys.exit(f"sha1 mismatch for {name}: expected {entry['sha1']}, got {actual}")
        print(f"sha1 ok: {actual}")

        extract = pathlib.Path(tmp) / "x"
        extract.mkdir()
        with tarfile.open(archive) as tar:
            tar.extractall(extract)

        roots = [p for p in extract.iterdir() if p.is_dir()]
        if len(roots) != 1:
            sys.exit(f"expected one top-level directory in {name}, found {len(roots)}")
        if dest.exists():
            shutil.rmtree(dest)
        shutil.move(str(roots[0]), str(dest))

    print(f"CEF {pin['cef_version']} ready at {dest}")


if __name__ == "__main__":
    main()
