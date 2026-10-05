# @icehomura/RecurAgent

One-line installer for the [RecurAgent](https://github.com/icehomura/RecurAgent) server — a
Rust-native, API-first Agentic OS.

```bash
npm install -g @icehomura/RecurAgent
ra serve
```

This package downloads the prebuilt release bundle for your platform and installs
the `ra` server **together with its bundled skills** (`news_fetch`,
`deep-search`, `deep_crawl`, `send_email`, `account_manager`, `voice`, `clock`,
`weather`). The skills are kept as siblings of the `ra` binary so that
`ra serve` can discover them at startup.

## Supported platforms

- macOS Apple Silicon (`darwin-arm64`)
- Linux x86_64 (`linux-x64`)
- Linux ARM64 (`linux-arm64`)
- Windows x64 (`win32-x64`)

macOS Intel is not supported (no prebuilt build is published).

## Environment overrides

- `ra_SKIP_DOWNLOAD=1` — skip the postinstall download (offline / CI).
- `ra_BUNDLE_URL=<url>` — install from a specific bundle URL (`file://` works).
- `HTTPS_PROXY` — honored when downloading.

## Alternatives

```bash
# Homebrew
brew install icehomura/RecurAgent/ra

# Shell installer (sets up ra serve as a service)
curl -fsSL https://github.com/icehomura/RecurAgent/releases/latest/download/install.sh | bash
```
