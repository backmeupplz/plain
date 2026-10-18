#!/usr/bin/env bash
# Turns the release tarballs into npm packages: plain-evm (a node shim that runs the binary) plus one
# plain-evm-<os>-<cpu> package per binary, which npm installs only on matching machines.
#   packaging/npm/build.sh <version> <dir with plain-<target>.tar.gz> <out dir>
# then `npm publish` every out/plain-evm-* directory first and out/plain-evm last.
set -euo pipefail
version=$1
dist=$(cd "$2" && pwd)
out=$3
here=$(cd "$(dirname "$0")" && pwd)

rm -rf "$out"
mkdir -p "$out"
cp -R "$here/plain-evm" "$out/plain-evm"
cp "$here/../../LICENSE" "$out/plain-evm/"
(cd "$out/plain-evm" && npm pkg set "version=$version")

for pair in darwin-arm64:aarch64-apple-darwin darwin-x64:x86_64-apple-darwin linux-x64:x86_64-unknown-linux-gnu; do
  platform=${pair%%:*} target=${pair#*:}
  os=${platform%-*} cpu=${platform#*-}
  name=plain-evm-$platform
  libc=
  [ "$os" = linux ] && libc='"libc": ["glibc"],'
  mkdir -p "$out/$name/bin"
  tar -xzf "$dist/plain-$target.tar.gz" -C "$out/$name/bin"
  cp "$here/../../LICENSE" "$out/$name/"
  cat > "$out/$name/package.json" <<EOF
{
  "name": "$name",
  "version": "$version",
  "description": "The $platform binary of plain-evm (https://getplain.org)",
  "repository": { "type": "git", "url": "git+https://github.com/backmeupplz/plain.git" },
  "license": "MIT",
  "os": ["$os"],
  "cpu": ["$cpu"],
  $libc
  "preferUnplugged": true
}
EOF
  (cd "$out/plain-evm" && npm pkg set "optionalDependencies.$name=$version")
done
