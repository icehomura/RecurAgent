.DEFAULT_GOAL := help

CARGO ?= cargo
FEATURES ?= api
OCTOS_DIR ?= $(CURDIR)/.ra
HOST ?= 127.0.0.1
PORT ?= 50080
SERVE_FLAGS ?= --solo

.PHONY: help init serve dashboard-build app-build dev test

help: ## Show available local-development commands.
	@awk 'BEGIN {FS = ":.*##"}; /^[a-zA-Z][a-zA-Z0-9_-]*:.*##/ {printf "  %-18s %s\n", $$1, $$2}' $(MAKEFILE_LIST)

init: ## Interactively create project-local .ra/config.json.
	$(CARGO) run -p ra-cli -- init --cwd "$(CURDIR)"

serve: ## Start the local API server (default: password-free local login).
	$(CARGO) run -p ra-cli --features "$(FEATURES)" -- serve --cwd "$(CURDIR)" --data-dir "$(OCTOS_DIR)" --host "$(HOST)" --port "$(PORT)" $(SERVE_FLAGS)

dashboard-build: ## Build the embedded /admin/ dashboard.
	./scripts/build-dashboard.sh

# There is no bundled web client in this fork: the upstream `octos-web` SPA
# submodule was removed, so `ra serve` answers 503 "web_bundle_missing" at /app
# until a client is built into crates/ra-cli/static/web/. Use `ra-tui` instead.
app-build: dashboard-build ## Build the embedded browser assets (/admin/).

dev: app-build serve ## Build browser assets, then start the local web app.

test: ## Run the Rust workspace test suite.
	$(CARGO) test --workspace
