SHELL := /bin/sh
export SOURCE_DATE_EPOCH ?= 0

.PHONY: fmt check test build reproducible release
fmt:
	cargo fmt --all

check:
	cargo fmt --all -- --check
	cargo clippy --locked --all-targets --all-features -- -D warnings

test:
	cargo test --locked --all-features

build:
	cargo build --release --locked

reproducible:
	./scripts/verify-reproducible.sh

release:
	./scripts/package-release.sh

