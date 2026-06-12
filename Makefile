.PHONY: fmt clippy build build-release build-small build-linux install run test check bump clean

APP_NAME := bnklaunch
TARGET := x86_64-unknown-linux-gnu
VERSION := $(shell grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
RELEASE_BIN := target/$(TARGET)/release/$(APP_NAME)
BIN_DIR := ~/.local/bin

# .cargo/config.toml sets -Cpanic=immediate-abort, which the test harness
# cannot build (it needs -Zpanic_abort_tests). Clearing the encoded rustflags
# drops that flag for any build that compiles the tests.
#
# RUST_MIN_STACK enlarges the stack of the threads libtest spawns. The heapless
# catalog is a multi-hundred-KB fixed buffer that lives on the stack, and the
# unoptimized test build moves it around without eliding copies, which overflows
# the default test thread stack.
#
# CARGO_UNSTABLE_BUILD_STD adds std back for the test build: the binary is no_std
# (build-std = ["core"] in config), but the test harness and the tests use std.
TEST_ENV := CARGO_ENCODED_RUSTFLAGS="" RUST_MIN_STACK=16777216 CARGO_UNSTABLE_BUILD_STD=std,core

fmt:
	cargo fmt --all

clippy:
	$(TEST_ENV) cargo clippy --all --benches --tests --examples --all-features -- -D warnings

build:
	cargo build

build-release:
	cargo build --release
	@ls -lh $(RELEASE_BIN)

# Size-optimized build (release-small profile in Cargo.toml).
build-small:
	cargo build --profile release-small
	@ls -lh target/$(TARGET)/release-small/$(APP_NAME)

# Install the release binary under ~/.local/bin, the same per-user location
# install.sh uses. bnklaunch is bound to a compositor hotkey, so it ships no
# desktop entry or icon.
install: build-release
	install -Dm755 $(RELEASE_BIN) $(BIN_DIR)/$(APP_NAME)
	@echo "Installed $(APP_NAME) to $(BIN_DIR)/$(APP_NAME)"

# Build a release tarball into dist/ for upload to a GitHub Release.
build-linux: build-release
	rm -rf dist/stage
	mkdir -p dist/stage
	cp $(RELEASE_BIN) dist/stage/$(APP_NAME)
	tar czf dist/$(APP_NAME)-v$(VERSION)-$(TARGET).tar.gz -C dist/stage .
	rm -rf dist/stage
	@echo "Built dist/$(APP_NAME)-v$(VERSION)-$(TARGET).tar.gz"
	@echo "Publish with: gh release create v$(VERSION) dist/$(APP_NAME)-v$(VERSION)-$(TARGET).tar.gz"

# Bump version, commit, and tag: make bump V=0.2.0
# Pushing the tag triggers the release workflow, which rejects any tag whose
# name does not match this version, so the two stay in lockstep.
bump:
	@test -n "$(V)" || (echo "Current: $(VERSION). Usage: make bump V=0.2.0" && exit 1)
	sed -i '0,/^version = ".*"/{s//version = "$(V)"/}' Cargo.toml
	cargo update --workspace
	git add Cargo.toml Cargo.lock
	git commit -m "Bump version to $(V)"
	git tag "v$(V)"
	@echo "Bumped to v$(V). Push with: git push origin main --tags"

run:
	cargo run

test:
	$(TEST_ENV) cargo test

check: fmt clippy test

clean:
	cargo clean
