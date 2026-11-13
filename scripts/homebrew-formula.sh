#!/usr/bin/env bash
# Prints the Homebrew formula for a release, using its SHA256SUMS.
#
# Usage: scripts/homebrew-formula.sh VERSION SHA256SUMS [BASE_URL]
#
# BASE_URL defaults to the GitHub release for VERSION; pass a file:// URL to
# test the formula against local archives built by scripts/dist.sh.
set -euo pipefail

if [ "$#" -lt 2 ] || [ "$#" -gt 3 ]; then
    echo "usage: $0 VERSION SHA256SUMS [BASE_URL]" >&2
    exit 2
fi
version=$1
sums=$2
base=${3:-https://github.com/delux-sh/delux/releases/download/v$version}

sha() {
    local archive="delux-$version-$1.tar.gz" sum
    sum=$(awk -v name="$archive" '$2 == name { print $1 }' "$sums") || return
    if ! [[ "$sum" =~ ^[0-9a-fA-F]{64}$ ]]; then
        echo "$0: $archive must have exactly one SHA256 in $sums" >&2
        exit 1
    fi
    printf '%s' "$sum"
}

archive() {
    local sum
    sum=$(sha "$1") || return
    printf '      url "%s/delux-%s-%s.tar.gz"\n      sha256 "%s"\n' "$base" "$version" "$1" "$sum"
}

# Validate every archive before emitting any formula. Errors inside a heredoc's
# command substitutions otherwise leave a successful formula with empty hashes.
macos_arm=$(archive aarch64-apple-darwin)
macos_intel=$(archive x86_64-apple-darwin)
linux_arm=$(archive aarch64-unknown-linux-musl)
linux_intel=$(archive x86_64-unknown-linux-musl)

cat <<EOF
class Delux < Formula
  desc "Find existing tmux sessions across your computers and attach to them over SSH"
  homepage "https://github.com/delux-sh/delux"
  license "GPL-3.0-or-later"
  depends_on "tmux"

  on_macos do
    on_arm do
$macos_arm
    end
    on_intel do
$macos_intel
    end
  end

  on_linux do
    on_arm do
$linux_arm
    end
    on_intel do
$linux_intel
    end
  end

  def install
    bin.install "delux"
    doc.install "README.md", "LICENSE",
                "THIRD-PARTY-NOTICES.txt", "RUST-NOTICES.html", "MUSL-COPYRIGHT"
  end

  test do
    assert_match "delux #{version}", shell_output("#{bin}/delux --version")
    ENV.delete "TMUX"
    ENV.delete "TMUX_PANE"
    ENV["XDG_CONFIG_HOME"] = testpath/"config"
    ENV["XDG_STATE_HOME"] = testpath/"state"
    ENV["TMUX_TMPDIR"] = testpath
    system "tmux", "-f", File::NULL, "new-session", "-d", "-s", "brew-test"
    begin
      inventory = JSON.parse(shell_output("#{bin}/delux list --local --json"))
      assert_equal ["brew-test"], inventory.fetch("computers").flat_map { |computer|
        computer.fetch("sessions").map { |session| session.fetch("name") }
      }
    ensure
      system "tmux", "kill-server"
    end
  end
end
EOF
