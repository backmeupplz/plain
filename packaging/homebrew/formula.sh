#!/usr/bin/env bash
# Prints the Homebrew formula for a release from its SHA256SUMS; the release workflow writes it to
# Formula/plain.rb in github.com/backmeupplz/homebrew-tap:
#   packaging/homebrew/formula.sh 0.2.0 SHA256SUMS > Formula/plain.rb
set -euo pipefail
version=$1
sums=$2
url=https://github.com/backmeupplz/plain/releases/download/v$version
sha() { awk -v f="plain-$1.tar.gz" '$2 == f || $2 == "*" f { print $1 }' "$sums" | grep .; }
mac_arm=$(sha aarch64-apple-darwin)
mac_intel=$(sha x86_64-apple-darwin)
linux_intel=$(sha x86_64-unknown-linux-gnu)

cat <<EOF
class Plain < Formula
  desc "Compile, test and deploy EVM smart contracts"
  homepage "https://getplain.org"
  version "$version"
  license "MIT"

  livecheck do
    url :stable
    strategy :github_latest
  end

  on_macos do
    on_arm do
      url "$url/plain-aarch64-apple-darwin.tar.gz"
      sha256 "$mac_arm"
    end
    on_intel do
      url "$url/plain-x86_64-apple-darwin.tar.gz"
      sha256 "$mac_intel"
    end
  end

  on_linux do
    on_intel do
      url "$url/plain-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "$linux_intel"
    end
  end

  def install
    bin.install "plain"
  end

  test do
    assert_equal "plain #{version}", shell_output("#{bin}/plain --version").strip
    system bin/"plain", "init"
    assert_path_exists testpath/"test/Counter.t.sol"
  end
end
EOF
