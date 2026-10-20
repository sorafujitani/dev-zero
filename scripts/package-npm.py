#!/usr/bin/env python3
"""Assemble the npm package from the four release archives; no network access."""
import json
import shutil
import sys
import tarfile
from pathlib import Path

import tomllib

ROOT = Path(__file__).resolve().parent.parent
TARGETS = {
    "aarch64-apple-darwin": "darwin-arm64",
    "x86_64-apple-darwin": "darwin-x64",
    "aarch64-unknown-linux-musl": "linux-arm64",
    "x86_64-unknown-linux-musl": "linux-x64",
}


def main():
    try:
        version = tomllib.loads((ROOT / "Cargo.toml").read_text())["package"]["version"]
        package = json.loads((ROOT / "npm/package.json").read_text())
    except (OSError, ValueError, KeyError) as error:
        raise SystemExit(f"Cannot read package manifests: {error}") from error
    if package["version"] != version:
        raise SystemExit("Cargo.toml and npm/package.json versions differ")
    archives = Path(sys.argv[1])
    for target, platform in TARGETS.items():
        archive = archives / f"dev-zero-v{version}-{target}.tar.gz"
        with tarfile.open(archive) as tar:
            member = tar.getmember("dz")
            if not member.isfile():
                raise SystemExit(f"{archive}: dz must be a regular file")
            destination = ROOT / "npm/prebuilt" / platform / "dz"
            destination.parent.mkdir(parents=True, exist_ok=True)
            source = tar.extractfile(member)
            assert source is not None, f"{archive}: dz has no contents"
            with source, destination.open("wb") as output:
                shutil.copyfileobj(source, output)
            destination.chmod(0o755)
    for name in ["README.md", "LICENSE"]:
        shutil.copy2(ROOT / name, ROOT / "npm" / name)


if __name__ == "__main__":
    main()
