.PHONY: fmt fmt-check clippy build build-release build-small build-linux install \
	run test perf perf-update scan-bench screenshot frame-update test-install \
	test-abi flatpak flatpak-lint check bump clean

APP_NAME := bnklaunch
APP_ID := io.github.borgenk.BnkLaunch
TARGET := x86_64-unknown-linux-gnu
VERSION := $(shell grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
RELEASE_BIN := target/$(TARGET)/release/$(APP_NAME)
BIN_DIR := ~/.local/bin
TARBALL := $(APP_NAME)-v$(VERSION)-$(TARGET).tar.gz
# flatpak-builder and its linter usually ship as the org.flatpak.Builder
# Flatpak. A copy on PATH wins.
FLATPAK_BUILDER := $(shell command -v flatpak-builder 2>/dev/null || echo "flatpak run org.flatpak.Builder")
FLATPAK_BUILDER_LINT := $(shell command -v flatpak-builder-lint 2>/dev/null \
	|| echo "flatpak run --command=flatpak-builder-lint org.flatpak.Builder")
LINT_EXCEPTIONS := --exceptions --user-exceptions .github/flatpak-lint-exceptions.json
# Where a bundle sends an installer that does not have the runtime. The release
# workflow's action embeds the same URL by default.
RUNTIME_REPO := https://flathub.org/repo/flathub.flatpakrepo

# The oldest glibc a downloaded tarball runs on, which is Debian 12's. The
# release builds in that image and checks the binary against this.
GLIBC_FLOOR := 2.36

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

fmt-check:
	cargo fmt --all -- --check

# Lint twice, because the two builds see different code. The test build has
# cfg(test) on, so anything only a test uses still counts as used; the binary
# build is what actually ships, and it is the one that finds code nothing calls.
# Neither pass alone catches everything.
clippy:
	$(TEST_ENV) cargo clippy --all --benches --tests --examples --all-features -- -D warnings
	cargo clippy --all-features -- -D warnings

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

# Build a release tarball into dist/ for upload to a GitHub Release, with the
# checksum install.sh verifies beside it.
build-linux: build-release
	sh .github/scripts/check-abi.sh $(GLIBC_FLOOR) $(RELEASE_BIN)
	rm -rf dist/stage
	mkdir -p dist/stage
	cp $(RELEASE_BIN) dist/stage/$(APP_NAME)
	tar czf dist/$(TARBALL) -C dist/stage .
	rm -rf dist/stage
	cd dist && sha256sum $(TARBALL) > $(TARBALL).sha256
	@echo "Built dist/$(TARBALL), with a .sha256"
	@echo "Publish with: gh release create v$(VERSION) dist/$(TARBALL)*"

# The bundle the release workflow publishes, into dist/.
flatpak:
	mkdir -p dist
	$(FLATPAK_BUILDER) --force-clean --repo=flatpak-repo build-dir $(APP_ID).yml
	flatpak build-bundle --runtime-repo=$(RUNTIME_REPO) flatpak-repo \
		dist/$(APP_NAME)-v$(VERSION)-x86_64.flatpak $(APP_ID)
	@echo "Built dist/$(APP_NAME)-v$(VERSION)-x86_64.flatpak"
	@echo "Install with: flatpak install --user dist/$(APP_NAME)-v$(VERSION)-x86_64.flatpak"

# What CI checks the manifest and the repo against. The accepted errors are in
# .github/flatpak-lint-exceptions.json.
flatpak-lint: flatpak
	$(FLATPAK_BUILDER_LINT) $(LINT_EXCEPTIONS) manifest $(APP_ID).yml
	$(FLATPAK_BUILDER_LINT) $(LINT_EXCEPTIONS) repo flatpak-repo

# Bump version, commit, and tag: make bump V=0.2.0
# Pushing the tag triggers the release workflow, which rejects any tag whose
# name does not match this version, so the two stay in lockstep.
bump:
	@test -n "$(V)" || (echo "Current: $(VERSION). Usage: make bump V=0.2.0" && exit 1)
	sed -i '0,/^version = ".*"/{s//version = "$(V)"/}' Cargo.toml
	sed -i 's|<releases>|<releases>\n    <release version="$(V)" date="'"$$(date -u +%F)"'"/>|' assets/$(APP_ID).metainfo.xml
	cargo update --workspace
	git add Cargo.toml Cargo.lock assets/$(APP_ID).metainfo.xml
	git commit -m "Bump version to $(V)"
	git tag "v$(V)"
	@echo "Bumped to v$(V). Push with: git push origin main --tags"

run:
	cargo run

test:
	$(TEST_ENV) cargo test

# The performance gate: the scenarios in src/dev/perf.rs, timed and compared against
# perf/baseline.txt. Built with optimizations, since timing a debug build says
# nothing about the binary anyone runs. Single-threaded, so the samples are not
# competing with each other for the machine.
perf:
	$(TEST_ENV) cargo test --profile perf -- --ignored --nocapture --test-threads=1 perf_gate

# Rewrite the baseline from this machine's numbers. Do this only for a change
# whose cost is deliberate, and say in the commit message what moved and why.
perf-update:
	BNKLAUNCH_PERF_UPDATE=1 $(TEST_ENV) cargo test --profile perf -- --ignored --nocapture --test-threads=1 perf_gate

# Where a cold start's time actually goes, measured against this machine's own
# application directories. Prints, never fails: the numbers are specific to what
# is installed here.
scan-bench:
	$(TEST_ENV) cargo test --profile perf -- --ignored --nocapture --test-threads=1 startup_stages

# Paint the scene in src/dev/screenshot.rs with the font this machine would use and
# write it to assets/screenshot.png, which is the image in the README.
screenshot:
	$(TEST_ENV) cargo test -- --ignored --nocapture write_screenshot

# Rewrite tests/fixtures/reference-frame.txt from the frame this machine draws.
# Do this only for a change to the UI that was meant.
frame-update:
	BNKLAUNCH_FRAME_UPDATE=1 $(TEST_ENV) cargo test -- --nocapture the_frame_matches_its_reference

# The installer's checksum verification, with no network.
test-install:
	sh .github/scripts/test-install.sh

# The ABI floor check, against binaries this machine already has.
test-abi:
	sh .github/scripts/test-abi.sh

# The gate. Checks and reports, never rewrites: fmt is the target that edits.
check: fmt-check clippy test test-install test-abi

clean:
	cargo clean
