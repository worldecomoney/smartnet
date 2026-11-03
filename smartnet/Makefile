# SmartNet node workspace — production binary: ../bin/smartnet
.DEFAULT_GOAL := help

REPO_ROOT := $(abspath ..)
ROOT_BIN_DIR := $(REPO_ROOT)/bin
GIT_SHA ?= $(shell git rev-parse HEAD 2>/dev/null || echo unknown)
GIT_TAG ?= $(shell git describe --tags --abbrev=0 2>/dev/null || echo dev)

CARGO_TARGET_DIR ?= target
export CARGO_TARGET_DIR := $(abspath $(CARGO_TARGET_DIR))

FEATURES ?=
PROFILE ?= release
# Cargo's built-in `dev` profile builds into target/debug (not target/dev); map it so the
# post-build copy below reads the right directory. Every other profile is its own dir name.
TARGET_SUBDIR := $(if $(filter dev,$(PROFILE)),debug,$(PROFILE))
EXTRA_RUSTFLAGS ?=
CARGO_INSTALL_EXTRA_FLAGS ?=
# Empty by default: online build. `make offline` / `make check` pass `--offline`.
CARGO_OFFLINE ?=

##@ Help

.PHONY: help
help: ## Display this help.
	@awk 'BEGIN {FS = ":.*##"; printf "Usage:\n  make \033[36m<target>\033[0m\n"} /^[a-zA-Z_0-9-]+:.*?##/ { printf "  \033[36m%-15s\033[0m %s\n", $$1, $$2 } /^##@/ { printf "\n\033[1m%s\033[0m\n", substr($$0, 5) } ' $(MAKEFILE_LIST)

##@ Build

.PHONY: install
install: ## Not here — use the repo root: make -C .. install
	@echo "Use: make -C .. install   # (build first, then install as root)"
	@exit 1

.PHONY: build
build: ## Build smartnet and copy to ../bin/smartnet (does not build validator).
	MAKEFLAGS= cargo build -p smartnet --bin smartnet --features "$(FEATURES)" --profile "$(PROFILE)" $(CARGO_OFFLINE)
	@mkdir -p "$(ROOT_BIN_DIR)"
	cp "$(CARGO_TARGET_DIR)/$(TARGET_SUBDIR)/smartnet" "$(ROOT_BIN_DIR)/smartnet"
	@echo "→ $(ROOT_BIN_DIR)/smartnet"

.PHONY: build-test-util
build-test-util: ## Build smartnet-test-util (target/ only; not installed to ../bin).
	cargo build -p smartnet-test-util --bin smartnet-test-util --profile "$(PROFILE)"

.PHONY: build-debug
build-debug: ## Build smartnet into target/debug/ (no ../bin copy).
	cargo build -p smartnet --bin smartnet --features "$(FEATURES)"

SOURCE_DATE ?= $(shell git log -1 --pretty=%ct 2>/dev/null || echo 0)
RUSTFLAGS_REPRODUCIBLE_EXTRA ?=

build-%-reproducible:
	@if [ "$*" != "smartnet" ]; then \
		echo "Error: reproducible builds only support smartnet, not $*"; \
		exit 1; \
	fi
	SOURCE_DATE_EPOCH=$(SOURCE_DATE) \
	RUSTFLAGS="-C symbol-mangling-version=v0 -C strip=none -C link-arg=-Wl,--build-id=none -C metadata='' --remap-path-prefix $$(pwd)=. $(RUSTFLAGS_REPRODUCIBLE_EXTRA)" \
	LC_ALL=C TZ=UTC \
	cargo build --bin smartnet --features "$(FEATURES)" --profile reproducible --locked --target x86_64-unknown-linux-gnu

build-native-%:
	$(if $(EXTRA_RUSTFLAGS),RUSTFLAGS="$(EXTRA_RUSTFLAGS)") \
	cargo build -p smartnet --bin smartnet --target $* --features "$(FEATURES)" --profile "$(PROFILE)"

##@ Test

UNIT_TEST_ARGS := --locked --workspace --features 'jemalloc-prof' -E 'kind(lib)' -E 'kind(bin)' -E 'kind(proc-macro)'
COV_FILE := lcov.info

.PHONY: test-unit
test-unit: ## Unit tests (lib/bin/proc-macro). Uses nextest when installed, else plain cargo test.
	@if cargo nextest --version >/dev/null 2>&1; then \
		cargo nextest run --no-fail-fast $(UNIT_TEST_ARGS); \
	else \
		echo "cargo-nextest not installed — running 'cargo test' instead"; \
		cargo test --workspace --lib --no-fail-fast $(CARGO_OFFLINE); \
	fi

.PHONY: cov-unit
cov-unit:
	rm -f $(COV_FILE)
	cargo llvm-cov nextest --lcov --output-path $(COV_FILE) $(UNIT_TEST_ARGS)

.PHONY: cov-report-html
cov-report-html: cov-unit
	cargo llvm-cov report --html
	open target/llvm-cov/html/index.html

##@ Other

.PHONY: clean
clean: ## Remove ../bin/smartnet only (keep cargo cache for offline builds).
	rm -f "$(ROOT_BIN_DIR)/smartnet"

.PHONY: check
check:
	cargo check -p smartnet --bin smartnet --features "$(FEATURES)" $(CARGO_OFFLINE)

.PHONY: profiling
profiling:
	RUSTFLAGS="-C target-cpu=native" cargo build -p smartnet --bin smartnet --profile profiling

.PHONY: maxperf
maxperf:
	RUSTFLAGS="-C target-cpu=native" cargo build -p smartnet --bin smartnet --profile maxperf

fmt:
	cargo +nightly fmt

# Read-only companion: reports what `make fmt` would change without writing.
fmt-check:
	cargo +nightly fmt --check

# Warning-budget gate instead of -D warnings: nightly-clippy counts drift with
# every nightly, so `clippy.budget` records the current count and this fails only
# when it *grows*. Ratchet the number down as warnings get fixed; raise it only
# with a reason (e.g. a nightly update added lints). Override per-run with
# CLIPPY_BUDGET=<n> (e.g. CLIPPY_BUDGET=0 make clippy for a strict pass).
clippy:
	@out=$$(cargo +nightly clippy --workspace --lib --examples --tests --benches --all-features 2>&1); ec=$$?; \
	if [ $$ec -ne 0 ]; then printf '%s\n' "$$out" | tail -40; exit $$ec; fi; \
	n=$$(printf '%s\n' "$$out" | grep -E '^warning:' | grep -vcE 'generated [0-9]+ warning'); \
	budget=$$(cat clippy.budget 2>/dev/null) || { echo "clippy.budget missing"; exit 1; }; \
	budget=$${CLIPPY_BUDGET:-$$budget}; \
	echo "clippy: $$n warning(s), budget $$budget"; \
	if [ $$n -gt $$budget ]; then \
		printf '%s\n' "$$out" | grep -E '^warning:' | grep -vE 'generated [0-9]+ warning' | head -40; \
		echo "clippy budget exceeded — fix warnings, or raise clippy.budget with a reason"; \
		exit 1; \
	fi

lint-typos: ensure-typos
	typos

ensure-typos:
	@command -v typos >/dev/null 2>&1 || { echo "install: cargo install --locked typos-cli"; exit 1; }

lint:
	$(MAKE) fmt && $(MAKE) clippy && $(MAKE) lint-typos

cargo-test:
	cargo test --workspace --lib --examples --tests --benches --all-features --no-fail-fast $(CARGO_OFFLINE)

test: ## Run the whole test suite (all features, lib + tests + benches + examples).
	$(MAKE) cargo-test
