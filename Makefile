SHELL := /bin/sh
export SOURCE_DATE_EPOCH ?= 0

.PHONY: fmt frontend check test build reproducible release package docker
fmt:
	cargo fmt --all

frontend:
	node --check admin-ui/src/build.mjs
	corepack pnpm --dir admin-ui install --frozen-lockfile
	corepack pnpm --dir admin-ui build

check:
	$(MAKE) frontend
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

package:
	cargo package --locked --allow-dirty

docker:
	docker build -t kiro-gateway-rs:local .
