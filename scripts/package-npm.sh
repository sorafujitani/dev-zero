#!/bin/sh
# Assemble the npm package from release archives; no network access.
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
archives=${1:?Usage: sh scripts/package-npm.sh ARCHIVE_DIRECTORY}
version=$(npm --prefix "$root/npm" pkg get version)
version=${version#\"}
version=${version%\"}
temporary=$(mktemp -d)
trap 'rm -rf "$temporary"' EXIT HUP INT TERM

for mapping in \
    aarch64-apple-darwin:darwin-arm64 \
    x86_64-apple-darwin:darwin-x64 \
    aarch64-unknown-linux-musl:linux-arm64 \
    x86_64-unknown-linux-musl:linux-x64
do
    target=${mapping%:*}
    platform=${mapping#*:}
    archive="$archives/dev-zero-v$version-$target.tar.gz"
    # Reject duplicate members and links; only stream the regular dz file.
    [ "$(tar -tzf "$archive" dz)" = dz ] || { echo "$archive: expected one dz member" >&2; exit 1; }
    case $(tar -tvzf "$archive" dz) in
        -*) ;;
        *) echo "$archive: dz must be a regular file" >&2; exit 1 ;;
    esac
    tar -xOzf "$archive" dz > "$temporary/$platform"
done

# Validate every archive before updating package contents.
for platform in darwin-arm64 darwin-x64 linux-arm64 linux-x64
do
    destination="$root/npm/prebuilt/$platform"
    mkdir -p "$destination"
    cp "$temporary/$platform" "$destination/dz"
    chmod 755 "$destination/dz"
done
cp "$root/README.md" "$root/LICENSE" "$root/npm/"
