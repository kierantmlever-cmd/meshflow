#!/usr/bin/env bash
# Cross-build a portable Windows 11 executable from Linux.
#
# The target has to be `x86_64-pc-windows-msvc`: Skia is only published prebuilt for the MSVC
# triple, and building it from source costs an hour and ~10 GB. `cargo-xwin` supplies the Windows
# SDK and CRT headers that target needs, downloading them from Microsoft on first use.
#
# Everything happens in a container so nothing is installed on the host, and the cargo caches are
# named volumes so a second run does not re-download the world.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="$REPO/dist"

# The build output goes to a bind-mounted directory in the repo, never a named volume: a volume
# would put the executable somewhere only another container can reach, while leaving an empty
# root-owned directory behind in the working tree. The caches *are* volumes — nobody needs to
# reach into those from the host, and keeping them makes a second run minutes rather than tens.
docker run --rm -t \
    -v "$REPO":/src -w /src \
    -v meshflow-cargo-registry:/usr/local/cargo/registry \
    -v meshflow-xwin:/root/.cache/cargo-xwin \
    -e "HOST_OWNER=$(id -u):$(id -g)" \
    rust:latest bash -eux -c '
        apt-get update -qq && apt-get install -y -qq clang llvm lld >/dev/null
        rustup target add x86_64-pc-windows-msvc
        cargo install cargo-xwin --locked
        # Static CRT: the point of a portable build is that it runs on a machine that has never
        # had a Visual C++ redistributable installed.
        RUSTFLAGS="-C target-feature=+crt-static" \
        CARGO_TARGET_DIR=/src/target-windows \
            cargo xwin build --release --target x86_64-pc-windows-msvc
        # The container is root and the repo is bind-mounted, so anything it writes lands in the
        # working tree owned by root and unopenable by the user who asked for the build. Handing
        # it back is part of the build, not an afterthought.
        chown -R "$HOST_OWNER" /src/target-windows
    '

EXE="$REPO/target-windows/x86_64-pc-windows-msvc/release/meshflow.exe"
if [ ! -f "$EXE" ]; then
    echo "the build produced no executable at $EXE" >&2
    exit 1
fi

mkdir -p "$OUT/meshflow"
cp "$EXE" "$OUT/meshflow/"
# Its presence is what makes the build portable — see `mf_engine::paths`.
mkdir -p "$OUT/meshflow/meshflow-data"
# Shared with the CI workflow rather than written twice — two copies of the same instructions
# drift, and the one nobody is looking at is the one that ends up in the zip.
cp "$REPO/dev/portable-README.txt" "$OUT/meshflow/README.txt"

cd "$OUT" && zip -qr meshflow-windows-x64.zip meshflow
echo "built: $OUT/meshflow-windows-x64.zip"
