#!/usr/bin/env bash
# Builds the signed apt and dnf/yum repositories served at $BASE_URL from every .deb and .rpm in <pkgs>:
#   packaging/build-repos.sh <pkgs dir> <out dir>
# Needs apt-utils (apt-ftparchive), createrepo-c and gpg with the signing key imported.
# Users then add:
#   apt: $BASE_URL/plain.sources (deb $BASE_URL/deb stable main, Signed-By $BASE_URL/plain.asc)
#   dnf: $BASE_URL/plain.repo
set -euo pipefail
pkgs=$(realpath "$1")
out=$(realpath -m "$2")
base=${BASE_URL:-https://pkg.getplain.org}
key=${GPG_KEY_ID:-$(gpg --list-secret-keys --with-colons | awk -F: '/^fpr/ {print $10; exit}')}
sign() { gpg --batch --yes --local-user "$key" --digest-algo SHA512 "$@"; }

rm -rf "$out"
mkdir -p "$out/deb/pool/main" "$out/deb/dists/stable/main/binary-amd64" "$out/rpm"
touch "$out/.nojekyll" # GitHub Pages: serve the files as they are
gpg --armor --export "$key" > "$out/plain.asc"

cp "$pkgs"/*.deb "$out/deb/pool/main/"
cd "$out/deb"
apt-ftparchive packages pool > dists/stable/main/binary-amd64/Packages
gzip -9kf dists/stable/main/binary-amd64/Packages
apt-ftparchive \
  -o APT::FTPArchive::Release::Origin=plain -o APT::FTPArchive::Release::Label=plain \
  -o APT::FTPArchive::Release::Suite=stable -o APT::FTPArchive::Release::Codename=stable \
  -o APT::FTPArchive::Release::Architectures=amd64 -o APT::FTPArchive::Release::Components=main \
  release dists/stable > "$out/Release.tmp" # written outside dists/ so it doesn't list itself
mv "$out/Release.tmp" dists/stable/Release
sign --clearsign --output dists/stable/InRelease dists/stable/Release
sign --armor --detach-sign --output dists/stable/Release.gpg dists/stable/Release

cp "$pkgs"/*.rpm "$out/rpm/"
cd "$out/rpm"
createrepo_c --quiet .
sign --armor --detach-sign --output repodata/repomd.xml.asc repodata/repomd.xml

cat > "$out/plain.sources" <<EOF
Types: deb
URIs: $base/deb
Suites: stable
Components: main
Signed-By: /etc/apt/keyrings/plain.asc
EOF
cat > "$out/plain.repo" <<EOF
[plain]
name=plain
baseurl=$base/rpm
enabled=1
gpgcheck=1
repo_gpgcheck=1
gpgkey=$base/plain.asc
EOF
