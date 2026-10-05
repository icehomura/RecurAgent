class Ra < Formula
  desc "Rust-native, API-first Agentic OS server (ra serve + bundled skills)"
  homepage "https://github.com/icehomura/RecurAgent"
  version "0.1.0"
  license "Apache-2.0"

  on_macos do
    on_arm do
      url "https://github.com/icehomura/RecurAgent/releases/download/v0.1.0/ra-bundle-aarch64-apple-darwin.tar.gz"
      sha256 "d6c3b53380a51579386687218feb575b4199a188061bad332a3541019a2107f4"
    end
    on_intel do
      odie "ra requires Apple Silicon; no x86_64 macOS build is published"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/icehomura/RecurAgent/releases/download/v0.1.0/ra-bundle-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "a28ca262a060e1e76862f4eda9af831d31a7f46de04a34e397737b17830f46da"
    end
    on_arm do
      url "https://github.com/icehomura/RecurAgent/releases/download/v0.1.0/ra-bundle-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "eb27475fde3fd823d23b460f6a8ab3f048f4bcde9cece22ab7bcab6f6d08eb8a"
    end
  end

  def install
    # The release bundle ships `ra` alongside its 8 skill binaries
    # (news_fetch, deep-search, deep_crawl, send_email, account_manager,
    # voice, clock, weather). At `ra serve` startup, bootstrap discovers
    # those skills as SIBLINGS of the resolved `ra` executable
    # (current_exe().parent()). Keep all of them together in libexec and
    # expose only `ra` on PATH.
    libexec.install Dir["*"]
    # Use an exec WRAPPER, not bin.install_symlink: on macOS,
    # std::env::current_exe() returns the *symlink* path (Darwin
    # _NSGetExecutablePath), so a bin/ra symlink would put exe_dir at bin/
    # and the sibling skills (in libexec) would not be found -> plugin_count=0.
    # `exec` replaces the process image, so current_exe() == libexec/ra and
    # exe_dir == libexec, where the skill binaries live.
    (bin/"ra").write <<~SH
      #!/bin/bash
      exec "#{libexec}/ra" "$@"
    SH
    # `Pathname#write` does not set the executable bit, so the wrapper — the only
    # `ra` on PATH — must be chmod'd or `ra` fails with permission denied.
    (bin/"ra").chmod 0755
  end

  test do
    assert_match "ra", shell_output("#{bin}/ra --version")
  end
end
