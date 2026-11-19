# Monorepo: calibri-lite (libs) + smartnet → bin/smartnet (validator merged in)
.PHONY: all build check clean clear offline install help
.DEFAULT_GOAL := all

ROOT := $(abspath $(dir $(lastword $(MAKEFILE_LIST))))
BIN_DIR := $(ROOT)/bin
PROFILE ?= release
# Config/keystore home of the *invoking* user. Under sudo, HOME is root's by default,
# which would park the config in /root/.smartnet — a path the real user's node never
# reads. Resolve the sudo caller's home via getent; fall back to $HOME when not sudo.
INSTALL_USER := $(if $(SUDO_USER),$(SUDO_USER),$(USER))
INSTALL_HOME := $(shell getent passwd "$(INSTALL_USER)" 2>/dev/null | cut -d: -f6)
ifeq ($(INSTALL_HOME),)
INSTALL_HOME := $(HOME)
endif
SMARTNET_DIR := $(INSTALL_HOME)/.smartnet
# User-invocable binary → /usr/local/bin (in every user's PATH; /usr/local/sbin is not).
SBIN_DIR := /usr/local/bin

##@ Help

help: ## Show targets.
	@awk 'BEGIN {FS = ":.*##"; printf "Usage:\n  make \033[36m<target>\033[0m\n"} /^[a-zA-Z_0-9-]+:.*?##/ { printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2 }' $(MAKEFILE_LIST)

##@ Build

all: build ## Build libs + app online (default goal, caches deps for offline).

build: ## Build libs + app; install into bin/.
	$(MAKE) -C calibri-lite build PROFILE="$(PROFILE)"
	$(MAKE) -C smartnet build PROFILE="$(PROFILE)"
	@echo "→ $(BIN_DIR)/smartnet"

offline: ## Build libs + app offline (uses cached deps).
	$(MAKE) -C calibri-lite build PROFILE="$(PROFILE)" CARGO_OFFLINE="--offline"
	$(MAKE) -C smartnet build PROFILE="$(PROFILE)" CARGO_OFFLINE="--offline"
	@echo "→ $(BIN_DIR)/smartnet (offline)"

install: ## Install config (as invoking user) + binary into /usr/local/bin (needs root).
	# Build first as your own user, then install under sudo, so root's PATH never needs cargo:
	#   make build && sudo make install
	# No datadir is created or assumed here: the data directory comes solely from
	# DATADIR_PATH in the config; `smartnet init` creates `<DATADIR_PATH>/<DB_ENGINE>`.
	@test -x "$(BIN_DIR)/smartnet" || \
		{ echo "error: binary missing in $(BIN_DIR) — run 'make build' first"; exit 1; }
	mkdir -p "$(SMARTNET_DIR)"
	mkdir -p "$(SBIN_DIR)"
	# The config home must be writable by the invoking user (keystore import rewrites
	# config.toml there) — a root-owned dir from a sudo run would block every later write.
	@if [ -n "$(SUDO_USER)" ]; then chown "$(SUDO_USER)" "$(SMARTNET_DIR)"; fi
	# One config for both roles: the node reads ~/.smartnet/config.toml of the invoking
	# user, and the merged validator section lives in that same file. Installed only when
	# missing — never overwrite an edited config.
	@if [ -e "$(SMARTNET_DIR)/config.toml" ]; then \
		echo "  keep existing $(SMARTNET_DIR)/config.toml"; \
	else \
		install -m 644 "$(ROOT)/smartnet/example.config.toml" "$(SMARTNET_DIR)/config.toml"; \
		echo "  new  $(SMARTNET_DIR)/config.toml"; \
	fi
	# Config belongs to the invoking user even when this runs under sudo.
	@if [ -n "$(SUDO_USER)" ] && [ -e "$(SMARTNET_DIR)/config.toml" ]; then \
		chown "$(SUDO_USER)" "$(SMARTNET_DIR)/config.toml"; \
	fi
	install -m 755 "$(BIN_DIR)/smartnet" "$(SBIN_DIR)/smartnet"
	@echo "→ $(SBIN_DIR)/smartnet"

check: ## cargo check in all workspaces.
	$(MAKE) -C calibri-lite check CARGO_OFFLINE="--offline"
	$(MAKE) -C smartnet check CARGO_OFFLINE="--offline"

clean: ## Remove bin/ binary + heavy debug artifacts (keep release cargo cache for offline rebuild).
	rm -f "$(BIN_DIR)/smartnet"
	rm -rf "$(ROOT)/calibri-lite/target/debug" \
	       "$(ROOT)/smartnet/target/debug" \
	       "$(ROOT)/target/debug"
	@echo "→ Removed binary + debug artifacts; release cargo cache preserved for offline rebuild"

clear: ## Full clean: remove binary + ALL cargo target dirs (drops the offline rebuild cache).
	rm -f "$(BIN_DIR)/smartnet"
	rm -rf "$(ROOT)/calibri-lite/target" \
	       "$(ROOT)/smartnet/target" \
	       "$(ROOT)/target"
	@echo "→ Full clean: binary + all cargo target dirs removed (offline rebuild cache dropped)"
