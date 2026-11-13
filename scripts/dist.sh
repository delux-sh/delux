#!/usr/bin/env bash
# Builds release archives into dist/: delux-<version>-<target>.tar.gz and SHA256SUMS.
#
# Usage: scripts/dist.sh [TARGET...]
#
# Without arguments, builds every target this host can produce. On macOS that
# is all four (macOS natively, Linux through cargo-zigbuild); on Linux, the
# two Linux targets. Linux binaries use musl and are statically linked, so they
# run without a distribution-specific libc. Requires rustup and cargo-about;
# Linux targets also need zig and cargo-zigbuild (see CONTRIBUTING.md).
set -euo pipefail

cd "$(dirname "$0")/.."

linux=(x86_64-unknown-linux-musl aarch64-unknown-linux-musl)
macos=(aarch64-apple-darwin x86_64-apple-darwin)
if [ "$#" -gt 0 ]; then
    targets=("$@")
elif [ "$(uname -s)" = Darwin ]; then
    targets=("${macos[@]}" "${linux[@]}")
else
    targets=("${linux[@]}")
fi

version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)
[ -n "$version" ] || { echo "dist: no version in Cargo.toml" >&2; exit 1; }

rm -rf dist
mkdir -p dist
cargo about generate --locked --fail scripts/licenses.hbs \
    --output-file dist/THIRD-PARTY-NOTICES.txt
rust_notices="$(rustc --print sysroot)/share/doc/rust/COPYRIGHT.html"
for target in "${targets[@]}"; do
    rustup target add "$target" >/dev/null
    case "$target" in
        *-linux-*) cargo zigbuild --locked --release --target "$target" ;;
        *) cargo build --locked --release --target "$target" ;;
    esac
    name="delux-$version-$target"
    mkdir "dist/$name"
    cp "target/$target/release/delux" README.md LICENSE "dist/$name/"
    cp dist/THIRD-PARTY-NOTICES.txt "dist/$name/"
    cp "$rust_notices" "dist/$name/RUST-NOTICES.html"
    cp licenses/musl-COPYRIGHT "dist/$name/MUSL-COPYRIGHT"
    # Record root ownership instead of the builder's account, and no macOS
    # extended attributes, which Linux tar reports as unknown headers.
    if tar --version 2>/dev/null | grep -q 'GNU tar'; then
        tar --owner=0 --group=0 --numeric-owner -C dist -czf "dist/$name.tar.gz" "$name"
    else
        tar --uid 0 --gid 0 --uname root --gname root --no-mac-metadata \
            -C dist -czf "dist/$name.tar.gz" "$name"
    fi
    rm -rf "dist/$name"
done
rm dist/THIRD-PARTY-NOTICES.txt
if command -v sha256sum >/dev/null; then sha256=(sha256sum); else sha256=(shasum -a 256); fi
(cd dist && "${sha256[@]}" ./*.tar.gz | sed 's| \./| |' > SHA256SUMS)
ls -l dist
