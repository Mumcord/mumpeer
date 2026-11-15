#!/usr/bin/env bash
# Writes one CycloneDX SBOM per release file whose contents differ:
#   mumpeer-<version>-linux-<arch>.cdx.json    the static binary in the .tar.gz
#   mumpeer-<version>-docker-<platform>.cdx.json  the Docker image (only with <image>)
# A binary SBOM lists the Rust crates compiled for that target (cargo-cyclonedx, without build
# and dev dependencies) and what the builder image links in statically: the Rust standard
# library, musl libc and Zig's runtime. The C code of ring and bovnar is built from those crates
# and is covered by their entries. An image SBOM lists the same and the Alpine packages of the
# image, read from its package database.
# The release workflow runs it; it also works locally. Usage:
#   scripts/generate-sbom.sh <version> <out-dir> [<image>]
# ARCHS limits the output, for example
#   ARCHS=x86_64 scripts/generate-sbom.sh 0.0.0 out mumpeer:dev
# Needs cargo-cyclonedx at the version in scripts/tools.env, docker and jq.
set -euo pipefail

version="${1:?usage: generate-sbom.sh <version> <out-dir> [<image>]}"
mkdir -p "${2:?out-dir missing}"
out_dir="$(cd "$2" && pwd)"
image="${3:-}"

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"
# shellcheck source=scripts/tools.env
source scripts/tools.env

fail() {
  echo "$1" >&2
  exit 1
}

command -v cargo-cyclonedx >/dev/null 2>&1 \
  || fail "cargo-cyclonedx not found. Install: cargo install cargo-cyclonedx --version $CARGO_CYCLONEDX_VERSION --locked"
for tool in docker jq; do
  command -v "$tool" >/dev/null 2>&1 || fail "$tool not found"
done

# Architecture, Rust target, Docker platform.
table="
x86_64   x86_64-unknown-linux-musl   amd64
aarch64  aarch64-unknown-linux-musl  arm64
"
archs="${ARCHS:-$(awk 'NF { print $1 }' <<<"$table")}"

tmp="$(mktemp -d)"
name="sbom-$$"
container=""
cleanup() {
  rm -rf "$tmp"
  # cargo-cyclonedx writes its file next to Cargo.toml.
  rm -f "$root/$name.json"
  if [ -n "$container" ]; then docker rm -f "$container" >/dev/null 2>&1 || true; fi
}
trap cleanup EXIT

# The binaries and the image are built in this image, so it is the source of the versions that
# are linked in statically. The release workflow reads it from the Dockerfile in the same way.
builder="$(sed -n 's/^FROM .* \([^ ]*cargo-zigbuild:[^ ]*\) AS build$/\1/p' Dockerfile)"
[ -n "$builder" ] || fail "builder image not found in the Dockerfile"

# cargo-zigbuild drops Rust's own musl and start files and links the ones of Zig.
# shellcheck disable=SC2016 # runs in the builder image
toolchain="$(docker run --rm "$builder" sh -c '
  echo "rust $(rustc --version | cut -d" " -f2)"
  echo "zig $(zig version)"
  zig_dir="$(dirname "$(readlink -f "$(command -v zig)")")"
  sed -n "s/^#define VERSION \"\(.*\)\"/musl \1/p" "$zig_dir/lib/libc/musl/src/internal/version.h"
')"
rust="$(awk '$1 == "rust" { print $2 }' <<<"$toolchain")"
zig="$(awk '$1 == "zig" { print $2 }' <<<"$toolchain")"
musl="$(awk '$1 == "musl" { print $2 }' <<<"$toolchain")"
[ -n "$rust" ] && [ -n "$zig" ] && [ -n "$musl" ] \
  || fail "Rust, Zig or musl version not found in $builder"

# A file must name the release and must not carry paths of the machine that made it.
check() {
  jq -e '.bomFormat == "CycloneDX" and .metadata.component.name == "mumpeer"
    and (.metadata.component.version | length > 0) and (.components | length > 0)' "$1" >/dev/null \
    || fail "$1 is not a complete SBOM"
  if grep -q 'file://' "$1"; then fail "$1 contains local paths"; fi
  echo "Wrote $1"
}

for arch in $archs; do
  read -r _ target platform < <(awk -v a="$arch" '$1 == a' <<<"$table") \
    || fail "unknown architecture: $arch"

  cargo-cyclonedx cyclonedx --format json --target "$target" --no-build-deps \
    --spec-version 1.5 --override-filename "$name" --quiet
  mv "$name.json" "$tmp/rust.json"

  # The crate carries the release version only once it is tagged, and cargo-cyclonedx names the
  # root by its local path: give it the version and a stable reference, then add the toolchain.
  binary="$out_dir/mumpeer-$version-linux-$arch.cdx.json"
  jq --arg version "$version" --arg arch "$arch" --arg builder "$builder" \
    --arg rust "$rust" --arg zig "$zig" --arg musl "$musl" '
    .metadata.component["bom-ref"] as $old
    | "mumpeer@\($version)" as $ref
    | [
        { type: "library", "bom-ref": "rust-std@\($rust)", name: "rust-std", version: $rust,
          description: "Rust standard library, linked statically",
          licenses: [{ expression: "MIT OR Apache-2.0" }],
          purl: "pkg:generic/rust-std@\($rust)" },
        { type: "library", "bom-ref": "musl@\($musl)", name: "musl", version: $musl,
          description: "musl libc as shipped with Zig \($zig), linked statically",
          licenses: [{ expression: "MIT" }],
          cpe: "cpe:2.3:a:musl-libc:musl:\($musl):*:*:*:*:*:*:*",
          purl: "pkg:generic/musl@\($musl)" },
        { type: "library", "bom-ref": "zig@\($zig)", name: "zig", version: $zig,
          description: "Zig toolchain that links the binary; its start files, compiler-rt and libunwind are linked statically",
          purl: "pkg:generic/zig@\($zig)" }
      ] as $native
    | .metadata.component |= (del(.components) | .["bom-ref"] = $ref | .version = $version
        | .purl = "pkg:github/mumcord/mumpeer@v\($version)")
    | .metadata.properties += [
        { name: "mumpeer:variant", value: "linux-\($arch)" },
        { name: "mumpeer:builder-image", value: $builder }
      ]
    | .components += $native
    | .dependencies |= (
        map(select(.ref | startswith($old + " ") | not))
        | map(if .ref == $old
            then .ref = $ref | .dependsOn += ($native | map(.["bom-ref"]))
            else . end)
        + ($native | map({ ref: .["bom-ref"] })))
  ' "$tmp/rust.json" > "$binary"
  check "$binary"

  [ -n "$image" ] || continue
  container="$(docker create --platform "linux/$platform" "$image")"
  docker cp "$container:/lib/apk/db/installed" - | tar -xO > "$tmp/apk"
  alpine="$(docker cp "$container:/etc/alpine-release" - | tar -xO)"
  docker rm "$container" >/dev/null
  container=""
  # The image is a multi-platform index pinned by digest; with the containerd image store,
  # pulling that same digest again for the next platform fails with "cannot overwrite digest"
  # while the previous platform's copy is still there.
  docker image rm "$image" >/dev/null 2>&1 || true
  [ -n "$alpine" ] || fail "Alpine version not found in $image"

  # One record per package, fields as "X:value" lines: name, version, architecture, licence, text.
  awk -F: -v OFS='\t' '
    function field() { return substr($0, 3) }
    /^P:/ { p = field() } /^V:/ { v = field() } /^A:/ { a = field() }
    /^L:/ { l = field() } /^T:/ { t = field() }
    /^$/ { if (p != "") print p, v, a, l, t; p = v = a = l = t = "" }
    END { if (p != "") print p, v, a, l, t }
  ' "$tmp/apk" > "$tmp/apk.tsv"
  [ -s "$tmp/apk.tsv" ] || fail "no Alpine packages found in $image"

  out="$out_dir/mumpeer-$version-docker-$platform.cdx.json"
  jq --arg version "$version" --arg platform "$platform" --arg alpine "$alpine" \
    --rawfile apk "$tmp/apk.tsv" '
    .metadata.component["bom-ref"] as $ref
    | ($apk | split("\n") | map(select(length > 0) | split("\t")) | map(
        { type: "library", "bom-ref": "apk:\(.[0])@\(.[1])", name: .[0], version: .[1],
          description: .[4],
          purl: "pkg:apk/alpine/\(.[0])@\(.[1])?arch=\(.[2])&distro=alpine-\($alpine)" }
        + (if .[3] != "" then { licenses: [{ expression: .[3] }] } else {} end)
      )) as $packages
    | .metadata.component |= (.type = "container"
        | .purl = "pkg:docker/mumcord/mumpeer@\($version)?repository_url=ghcr.io&arch=\($platform)")
    | .metadata.properties |= map(if .name == "mumpeer:variant"
        then .value = "docker-\($platform)" else . end)
    | .metadata.properties += [{ name: "mumpeer:base-image", value: "alpine:\($alpine)" }]
    | .components += $packages
    | .dependencies |= (
        map(if .ref == $ref then .dependsOn += ($packages | map(.["bom-ref"])) else . end)
        + ($packages | map({ ref: .["bom-ref"] })))
  ' "$binary" > "$out"
  check "$out"
done
