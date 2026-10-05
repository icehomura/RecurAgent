# Prerelease-channel formula for ra-tui (class RaTuiDev).
#
# This is the DEV/prerelease sibling of Formula/ra-tui.rb. It is rendered by
# .github/workflows/publish-homebrew.yml on a PRERELEASE tag push (a tag with a
# '-', e.g. v0.2.2-rc.15) into Formula/ra-tui-dev.rb, filling the same
# 0.3.0-rc.11/v0.3.0-rc.11/__SHA_*__ placeholders from that prerelease's assets. The
# stable Formula/ra-tui.rb is NEVER touched by a prerelease tag, so
# `brew install icehomura/ra-tui/ra-tui` stays on the latest STABLE while
# `brew install icehomura/ra-tui/ra-tui-dev` tracks the latest prerelease.
#
# MUTUALLY EXCLUSIVE with the stable formula: both install a binary named
# `ra-tui`, so only one may be linked at a time (see `conflicts_with` below).
# This is the standard `foo` vs `foo-dev` pattern — install one or the other.
class RaTuiDev < Formula
  desc "Terminal UI client for the ra UI Protocol (prerelease channel)"
  homepage "https://github.com/icehomura/ra-tui"
  version "0.1.0"
  if OS.mac? && Hardware::CPU.arm?
    url "https://github.com/icehomura/ra-tui/releases/download/v0.3.0-rc.11/ra-tui-aarch64-apple-darwin.tar.xz"
    sha256 "0f91572b5b349fef0fda5f1e9f36ccfa1d7f102d6e165c35e3debb046bf3f735"
  end
  if OS.linux?
    if Hardware::CPU.arm?
      url "https://github.com/icehomura/ra-tui/releases/download/v0.3.0-rc.11/ra-tui-aarch64-unknown-linux-gnu.tar.xz"
      sha256 "1cce50b57dbd75b8d546df51c6589706ef190e47a6567a6d21b11dfdc5b9a3a1"
    end
    if Hardware::CPU.intel?
      url "https://github.com/icehomura/ra-tui/releases/download/v0.3.0-rc.11/ra-tui-x86_64-unknown-linux-gnu.tar.xz"
      sha256 "e56a23a674938a3f010f938fc1c6a563ed057c7c06c7426028cf8d76fdcf02b2"
    end
  end
  license "Apache-2.0"

  # Dev and stable both provide `bin/ra-tui`; they cannot be linked together.
  # Installing this formula while `ra-tui` is linked (or vice versa) prompts
  # to `brew unlink` the other first, keeping the two channels cleanly separate.
  conflicts_with "ra-tui", because: "both install the ra-tui binary (prerelease vs stable channel)"

  # ra-tui is a CLIENT; a local launch spawns `ra serve --stdio` as its
  # backend. We deliberately do NOT `depends_on "icehomura/ra/ra"`: Homebrew
  # does not auto-tap third-party dependency taps, so that would abort the
  # install with "tap must be installed explicitly". Instead the tui
  # auto-installs the ra server on first run if it's missing (see caveats).
  def caveats
    <<~EOS
      ra-tui-dev is the PRERELEASE (rc/beta) channel; the stable formula is
      `icehomura/ra-tui/ra-tui`. Only one may be linked at a time.

      ra-tui talks to the `ra` server backend. If ra isn't installed,
      ra-tui installs the latest release automatically on first run
      (set RA_TUI_NO_AUTO_INSTALL=1 to disable). To install it up front:
        brew install icehomura/ra/ra
    EOS
  end

  BINARY_ALIASES = {
    "aarch64-apple-darwin":      {},
    "aarch64-unknown-linux-gnu": {},
    "x86_64-pc-windows-gnu":     {},
    "x86_64-unknown-linux-gnu":  {},
  }.freeze

  def target_triple
    cpu = Hardware::CPU.arm? ? "aarch64" : "x86_64"
    os = OS.mac? ? "apple-darwin" : "unknown-linux-gnu"

    "#{cpu}-#{os}"
  end

  def install_binary_aliases!
    BINARY_ALIASES[target_triple.to_sym].each do |source, dests|
      dests.each do |dest|
        bin.install_symlink bin/source.to_s => dest
      end
    end
  end

  def install
    bin.install "ra-tui" if OS.mac? && Hardware::CPU.arm?
    bin.install "ra-tui" if OS.linux? && Hardware::CPU.arm?
    bin.install "ra-tui" if OS.linux? && Hardware::CPU.intel?

    install_binary_aliases!

    # Homebrew will automatically install these, so we don't need to do that
    doc_files = Dir["README.*", "readme.*", "LICENSE", "LICENSE.*", "CHANGELOG.*"]
    leftover_contents = Dir["*"] - doc_files

    # Install any leftover files in pkgshare; these are probably config or
    # sample files.
    pkgshare.install(*leftover_contents) unless leftover_contents.empty?
  end
end
