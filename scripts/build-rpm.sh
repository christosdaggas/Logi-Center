#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
for tool in cargo rpmbuild rpm python3 tar find install sha256sum; do
    command -v "$tool" >/dev/null || { echo "Missing build tool: $tool" >&2; exit 1; }
done
VERSION="$(python3 -c 'import tomllib; print(tomllib.load(open("Cargo.toml", "rb"))["package"]["version"])')"
OUT="$ROOT/dist/rpm"
mkdir -p "$OUT" "$ROOT/target/rpm"
WORK="$(mktemp -d "$ROOT/target/rpm/build.XXXXXX")"
trap 'rm -rf -- "$WORK"' EXIT

export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=$HOME=/usr/src/build --remap-path-prefix=$ROOT=/usr/src/logicenter --remap-path-prefix=$HOME/.cargo=/usr/src/cargo"
cargo build --release --locked
mkdir -p "$WORK"/{BUILD,BUILDROOT,RPMS,SOURCES,SPECS,SRPMS}
STAGE="$WORK/logicenter-$VERSION"
mkdir -p "$STAGE"
install -m0755 target/release/logicenter "$STAGE/logicenter"
install -m0644 README.md CHANGELOG.md LICENSE packaging/com.chrisdaggas.logicenter.desktop packaging/70-logicenter.rules docs/logo.svg docs/symbolic.svg "$STAGE/"
tar -C "$WORK" -czf "$WORK/SOURCES/logicenter-$VERSION.tar.gz" "logicenter-$VERSION"
rpmbuild -bb "$ROOT/packaging/logicenter.spec" --define "_topdir $WORK" --define "app_version $VERSION" --define "_buildhost fedora-builder"
mapfile -t PACKAGES < <(find "$WORK/RPMS" -type f -name '*.rpm' ! -name '*debuginfo*' ! -name '*debugsource*')
if [[ ${#PACKAGES[@]} -eq 0 ]]; then
    echo "rpmbuild produced no package" >&2
    exit 1
fi
for package in "${PACKAGES[@]}"; do
    install -m0644 "$package" "$OUT/$(basename "$package")"
    (cd "$OUT" && sha256sum "$(basename "$package")" > "$(basename "$package").sha256")
    (cd "$OUT" && sha256sum --check "$(basename "$package").sha256")
    rpm --checksig "$OUT/$(basename "$package")"
    echo "RPM saved: $OUT/$(basename "$package")"
done
