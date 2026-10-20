#!/usr/bin/env python3
"""Verify npm assembly and offline installation using native binaries as fixtures."""
import json
import os
import shutil
import subprocess
import sys
import tarfile
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def run(*args, cwd):
    return subprocess.check_output(args, cwd=cwd, text=True)


def main():
    binary = Path(sys.argv[1]).resolve()
    with tempfile.TemporaryDirectory(prefix="dev-zero-package-") as temporary:
        root = Path(temporary)
        for name in ["Cargo.toml", "README.md", "LICENSE", "scripts/package-npm.py", "npm/package.json", "npm/bin/dz.cjs"]:
            target = root / name
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(ROOT / name, target)
        version = json.loads((root / "npm/package.json").read_text())["version"]
        dist = root / "dist"
        dist.mkdir()
        # Only the native binary is executed. Other target archives are assembly fixtures.
        for target in ["aarch64-apple-darwin", "x86_64-apple-darwin", "aarch64-unknown-linux-musl", "x86_64-unknown-linux-musl"]:
            with tarfile.open(dist / f"dev-zero-v{version}-{target}.tar.gz", "w:gz") as archive:
                archive.add(binary, arcname="dz")
        run(sys.executable, "scripts/package-npm.py", str(dist), cwd=root)
        packed = json.loads(run("npm", "pack", "./npm", "--json", "--pack-destination", str(dist), cwd=root))[0]
        paths = {file["path"] for file in packed["files"]}
        for platform in ["darwin-arm64", "darwin-x64", "linux-arm64", "linux-x64"]:
            assert f"prebuilt/{platform}/dz" in paths
        assert {"README.md", "LICENSE", "bin/dz.cjs"} <= paths
        assert not any(path.startswith("test/") for path in paths)
        install = root / "installed"
        run("npm", "install", "--offline", "--ignore-scripts", "--no-audit", "--no-fund", "--prefix", str(install), str(dist / packed["filename"]), cwd=root)
        launcher = install / "node_modules/.bin/dz"
        assert os.access(launcher, os.X_OK)
        assert run(str(launcher), "--version", cwd=root).strip() == f"dz {version}"
        assert "dev-null.sh" in run(str(launcher), "--help", cwd=root)
        failed = subprocess.run([str(launcher), "unknown-command"], cwd=root, capture_output=True, check=False)
        assert failed.returncode == 2
        assert failed.stdout == b""
    print("npm package smoke test passed (contents, offline install, arguments, exit status).")


if __name__ == "__main__":
    main()
