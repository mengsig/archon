.PHONY: fmt fmt-check lint shell doc test test-nextest build install ci e2e
export CARGO_BUILD_JOBS ?= 4

fmt:       ; cargo fmt --all
fmt-check: ; cargo fmt --all -- --check
SHELL_SCRIPTS := install.sh scripts/package-release.sh scripts/release-notes.sh scripts/build-release-local.sh scripts/test-build-release-local.sh
# The shell scripts are linted with the Rust code, so CI's lint job covers them too.
lint:      shell ; cargo clippy --workspace --all-targets --locked -- -D warnings
shell:     ; for script in $(SHELL_SCRIPTS); do sh -n "$$script" || exit 1; done; shellcheck $(SHELL_SCRIPTS) && sh scripts/test-build-release-local.sh
doc:       ; RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked
test:      ; cargo test --workspace --locked --no-fail-fast
# The tests of `test` through cargo-nextest, which runs the test binaries in
# parallel; it skips doctests, so those run after. CI uses this target.
test-nextest:
	cargo nextest run --workspace --locked --no-fail-fast
	cargo test --workspace --locked --doc
build:     ; cargo build --workspace --locked
install:   ; cargo install --locked --path crates/archon
ci:        fmt-check lint doc test
# The mobile web app in headless Chromium against a real archond; installs its
# npm packages and the browser first. Run for releases; not part of `ci`.
e2e:
	cargo build -p archon --locked
	ARCHON_BIN="$${CARGO_TARGET_DIR:-$(CURDIR)/target}/debug/archon" node crates/archon-remote/web/scripts/e2e.js
