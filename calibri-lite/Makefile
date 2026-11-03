# Protocol libraries only — no binaries (root/smartnet make build depends on this).
PROFILE ?= release
# Empty by default: `make build` (root) is the ONLINE build. The root `offline`
# target and `make check` pass `--offline` explicitly. Use `make offline` to
# rebuild from the cached deps without touching the network.
CARGO_OFFLINE ?=

export CARGO_TARGET_DIR := $(abspath target)

.PHONY: build clean check

build:
	cargo build --workspace --profile "$(PROFILE)" $(CARGO_OFFLINE)

clean:
	cargo clean

check:
	cargo check --workspace $(CARGO_OFFLINE)
