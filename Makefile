# crash_monitor — self-contained build / lint / test / coverage.
#
# Works standalone from a clone of just this repository, including the
# end-to-end tests: their crash-producing child (tests/e2e/fixtures/crash_app.c)
# depends only on the shm schema, not on any host application.
#
# Common overrides:
#   make sign SIGN_IDENTITY="Developer ID Application: ..."  # different signer
#   CRASH_MONITOR_DATA_DIR_NAME=.myapp make sign              # bake a host default

.DEFAULT_GOAL := build-unsigned

# Codesigning identity for the debugger entitlement (task_for_pid, vm_read).
# Without it the monitor cannot inspect a crashing child (and e2e self-skips).
SIGN_IDENTITY ?= Apple Development
SIGN_IDENTITY_RESOLVER := ./scripts/resolve-sign-identity.sh
ENTITLEMENTS  := crash_monitor.entitlements
DIALOG_ENTITLEMENTS := crash_dialog.entitlements

MONITOR_BIN        := target/release/crash_monitor
MONITOR_DIALOG_BIN := target/release/crash_dialog_macos

# Self-contained e2e crash producer (schema-only; no host app dependency).
E2E_CHILD := tests/e2e/fixtures/crash_app
E2E_SRC   := tests/e2e/fixtures/crash_app.c
SHM_ATOMIC_TEST     := target/shm_atomic_contract_test
SHM_ATOMIC_TEST_SRC := tests/e2e/fixtures/shm_atomic_contract.c
PRODUCER_SDK_TEST := target/producer_sdk_contract_test
PRODUCER_SDK_TEST_SRC := tests/e2e/fixtures/producer_sdk_contract.c

# cargo-llvm-cov discovers rustup's llvm-tools-preview. LLVM_COV and
# LLVM_PROFDATA remain supported as explicit caller-provided overrides.
# Do not hide main/FFI/path code from the denominator. Signed E2E coverage
# executes those boundaries in the real monitor process; remaining lines are
# reported as uncovered rather than silently excluded.
COV_EXCLUDE ?=
COV_ENV_FILE := target/llvm-cov.env

.PHONY: build build-unsigned check-sign-identity sign sign-adhoc package verify-package e2e-build lint test \
        unit-test integration-test e2e e2e-test e2e-required e2e-child shm-atomic-test \
        producer-sdk-test schema-check ci-fast coverage unit-coverage \
        integration-coverage e2e-coverage clean

# ── Compile / sign / package ──────────────────────────────────
build-unsigned:
	cargo build --release --workspace

# Backward-compatible compile-only alias. Signing is always explicit.
build: build-unsigned

check-sign-identity:
	@$(SIGN_IDENTITY_RESOLVER) "$(SIGN_IDENTITY)" >/dev/null

# Check credentials before starting a potentially long release compilation.
sign: check-sign-identity
	$(MAKE) build-unsigned
	@resolved_identity="$$($(SIGN_IDENTITY_RESOLVER) "$(SIGN_IDENTITY)")" && \
		codesign --entitlements $(ENTITLEMENTS) --force --sign "$$resolved_identity" $(MONITOR_BIN) && \
		codesign --entitlements $(DIALOG_ENTITLEMENTS) --force --sign "$$resolved_identity" $(MONITOR_DIALOG_BIN)

# Ad-hoc signing is suitable for local distribution checks only. It does not
# grant task_for_pid and therefore cannot run privileged capture E2E tests.
sign-adhoc: build-unsigned
	codesign --force --sign - $(MONITOR_BIN)
	codesign --force --sign - $(MONITOR_DIALOG_BIN)

package: sign
	./scripts/package-release.sh target/package $(MONITOR_BIN) $(MONITOR_DIALOG_BIN)

verify-package: package
	./scripts/verify-release.sh target/package/crash-monitor.tar.gz

# E2E alone enables the mock-dialog environment override. Production `build`
# never compiles that trust-boundary bypass.
e2e-build: check-sign-identity
	cargo build --release --workspace --features test-support
	cargo build --release --locked --manifest-path crates/crash_dialog_mock/Cargo.toml --target-dir target
	@resolved_identity="$$($(SIGN_IDENTITY_RESOLVER) "$(SIGN_IDENTITY)")" && \
		codesign --entitlements $(ENTITLEMENTS) --force --sign "$$resolved_identity" $(MONITOR_BIN) && \
		codesign --entitlements $(DIALOG_ENTITLEMENTS) --force --sign "$$resolved_identity" $(MONITOR_DIALOG_BIN)

# ── Lint ──────────────────────────────────────────────────────
lint:
	cargo fmt --all -- --check
	cargo clippy --workspace --all-targets --all-features -- -D warnings

ci-fast: schema-check
	cargo fmt --all -- --check
	cargo clippy --workspace --all-targets --all-features -- -D warnings
	cargo test --workspace --all-targets

# ── E2E child (compiled from the shm schema alone) ────────────
$(E2E_CHILD): $(E2E_SRC) schema/crash_shm.h schema/crash_shm_atomic.h
	cc -std=c11 -Wall -Wextra -Werror -I schema -o $@ $<

e2e-child: $(E2E_CHILD)

$(SHM_ATOMIC_TEST): $(SHM_ATOMIC_TEST_SRC) schema/crash_shm.h schema/crash_shm_atomic.h
	mkdir -p $(@D)
	cc -std=c11 -Wall -Wextra -Werror -I schema -o $@ $<

shm-atomic-test: $(SHM_ATOMIC_TEST)
	./$(SHM_ATOMIC_TEST)

$(PRODUCER_SDK_TEST): $(PRODUCER_SDK_TEST_SRC) producer/crash_monitor_producer.h schema/crash_shm.h schema/crash_shm_atomic.h
	mkdir -p $(@D)
	cc -std=c11 -Wall -Wextra -Werror -I schema -I producer -o $@ $<

producer-sdk-test: $(PRODUCER_SDK_TEST)
	./$(PRODUCER_SDK_TEST)

schema-check: shm-atomic-test producer-sdk-test
	cargo check --workspace --all-targets

# ── Tests ─────────────────────────────────────────────────────
unit-test: shm-atomic-test producer-sdk-test
	cargo test --lib

integration-test:
	cargo test --workspace --tests

# e2e needs the codesigned release monitor (build), the child, and a debug build
# (for the unsigned-fails-fast case). Tests self-skip if the entitlement is absent.
e2e-test: e2e-build $(E2E_CHILD)
	cargo build
	cargo test --test e2e_tests

# Privileged release gate. The dedicated runner must provide a signing identity
# capable of granting com.apple.security.cs.debugger.
e2e-required: e2e-build $(E2E_CHILD)
	cargo build
	E2E_REQUIRED=1 cargo test --test e2e_tests -- --include-ignored --test-threads=1

test: schema-check
	cargo test --workspace --all-targets

e2e: e2e-required

# ── Coverage (HTML reports under coverage-report*/) ───────────
unit-coverage:
	cargo llvm-cov --lib $(COV_EXCLUDE) --html --output-dir coverage-report-unit
	@echo "Unit coverage: coverage-report-unit/html/index.html"

integration-coverage:
	cargo llvm-cov --workspace --tests $(COV_EXCLUDE) --html --output-dir coverage-report-integration
	@echo "Integration coverage: coverage-report-integration/html/index.html"

e2e-coverage: check-sign-identity $(E2E_CHILD)
	cargo llvm-cov clean --workspace
	mkdir -p target
	cargo llvm-cov show-env --sh > $(COV_ENV_FILE)
	. $(COV_ENV_FILE); cargo build --release --workspace --features test-support
	@resolved_identity="$$($(SIGN_IDENTITY_RESOLVER) "$(SIGN_IDENTITY)")" && \
		codesign --entitlements $(ENTITLEMENTS) --force --sign "$$resolved_identity" $(MONITOR_BIN) && \
		codesign --entitlements $(DIALOG_ENTITLEMENTS) --force --sign "$$resolved_identity" $(MONITOR_DIALOG_BIN)
	. $(COV_ENV_FILE); CRASH_MONITOR_E2E_BIN=$(abspath $(MONITOR_BIN)) E2E_REQUIRED=1 cargo test --test e2e_tests -- --include-ignored --test-threads=1
	cargo llvm-cov report $(COV_EXCLUDE) --html --output-dir coverage-report-e2e
	@echo "E2E coverage: coverage-report-e2e/html/index.html"

coverage: build $(E2E_CHILD)
	cargo llvm-cov --workspace --all-targets $(COV_EXCLUDE) --html --output-dir coverage-report
	@echo "Coverage: coverage-report/html/index.html"

# ── Clean ─────────────────────────────────────────────────────
clean:
	cargo clean
	rm -f $(E2E_CHILD)
	rm -rf coverage-report coverage-report-unit coverage-report-integration coverage-report-e2e
