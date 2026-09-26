SHELL := /bin/sh
export SOURCE_DATE_EPOCH ?= 0

.PHONY: fmt frontend secrets check test client-smoke build reproducible release package docker audit deny
fmt:
	cargo fmt --all

frontend:
	node --check admin-ui/src/build.mjs
	corepack pnpm@9.15.4 --dir admin-ui install --frozen-lockfile
	corepack pnpm@9.15.4 --dir admin-ui build

secrets:
	./scripts/check-no-secrets.sh
	./tests/check-no-secrets.sh

check:
	$(MAKE) frontend
	$(MAKE) secrets
	cargo fmt --all -- --check
	cargo clippy --locked --all-targets --all-features -- -D warnings

test:
	cargo test --locked --all-features

client-smoke:
	./scripts/test-local-clients.sh

build:
	cargo build --release --locked

reproducible:
	./scripts/verify-reproducible.sh

release:
	./scripts/package-release.sh

package:
	cargo package --locked --allow-dirty

docker:
	docker build -t kiro-gateway:local .

audit:
	cargo audit

deny:
	cargo deny --locked check
