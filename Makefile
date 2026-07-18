# crash_monitor — self-contained build / lint / test / coverage.
#
# Works standalone from a clone of just this repository, including the
# end-to-end tests: their crash-producing child (tests/e2e/fixtures/crash_app.c)
# depends only on the shm schema, not on any host application.
#
# Common overrides:
#   make build SIGN_IDENTITY="Developer ID Application: ..."   # different signer
#   CRASH_MONITOR_DATA_DIR_NAME=.myapp make build             # bake a host default

.DEFAULT_GOAL := build

# Codesigning identity for the debugger entitlement (task_for_pid, vm_read).
# Without it the monitor cannot inspect a crashing child (and e2e self-skips).
SIGN_IDENTITY ?= Apple Development
ENTITLEMENTS  := crash_monitor.entitlements
DIALOG_ENTITLEMENTS := crash_dialog.entitlements

MONITOR_BIN        := target/release/crash_monitor
MONITOR_DIALOG_BIN := target/release/crash_dialog_macos

# Self-contained e2e crash producer (schema-only; no host app dependency).
E2E_CHILD := tests/e2e/fixtures/crash_app
E2E_SRC   := tests/e2e/fixtures/crash_app.c
SHM_ATOMIC_TEST     := target/shm_atomic_contract_test
SHM_ATOMIC_TEST_SRC := tests/e2e/fixtures/shm_atomic_contract.c

# Homebrew LLVM tools for cargo-llvm-cov (macOS).
LLVM_COV_ENV := LLVM_COV=/opt/homebrew/opt/llvm/bin/llvm-cov \
                LLVM_PROFDATA=/opt/homebrew/opt/llvm/bin/llvm-profdata
# Exclude untestable code from coverage:
#   platform/.*/ffi/ — platform FFI (macos/ffi/, future linux/ffi/)
#   main.rs          — OS orchestration (signal, spawn, waitpid)
#   paths.rs         — OS I/O (env::var, fs::create_dir_all)
#   platform/mod.rs  — FFI delegation wrappers
COV_EXCLUDE := --ignore-filename-regex '(platform/.*/ffi/|/main\.rs$$|/paths\.rs$$|platform/mod\.rs$$)'

.PHONY: build e2e-build lint test unit-test integration-test e2e-test e2e-required \
        e2e-child shm-atomic-test schema-check ci-fast coverage unit-coverage \
        integration-coverage e2e-coverage clean

# ── Build (release + codesign) ────────────────────────────────
build:
	cargo build --release --workspace
	codesign --entitlements $(ENTITLEMENTS) --force --sign "$(SIGN_IDENTITY)" $(MONITOR_BIN)
	codesign --entitlements $(DIALOG_ENTITLEMENTS) --force --sign "$(SIGN_IDENTITY)" $(MONITOR_DIALOG_BIN)

# E2E alone enables the mock-dialog environment override. Production `build`
# never compiles that trust-boundary bypass.
e2e-build:
	cargo build --release --workspace --features test-support
	codesign --entitlements $(ENTITLEMENTS) --force --sign "$(SIGN_IDENTITY)" $(MONITOR_BIN)
	codesign --entitlements $(DIALOG_ENTITLEMENTS) --force --sign "$(SIGN_IDENTITY)" $(MONITOR_DIALOG_BIN)

# ── Lint ──────────────────────────────────────────────────────
lint:
	cargo fmt -- --check
	cargo clippy -- -D warnings -A dead_code

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

schema-check: shm-atomic-test
	cargo check --workspace --all-targets

# ── Tests ─────────────────────────────────────────────────────
unit-test: shm-atomic-test
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
	E2E_REQUIRED=1 cargo test --test e2e_tests -- --ignored

test: schema-check
	cargo test --workspace --all-targets

# ── Coverage (HTML reports under coverage-report*/) ───────────
unit-coverage:
	$(LLVM_COV_ENV) cargo llvm-cov --lib $(COV_EXCLUDE) --html --output-dir coverage-report-unit
	@echo "Unit coverage: coverage-report-unit/html/index.html"

integration-coverage:
	$(LLVM_COV_ENV) cargo llvm-cov --workspace --tests $(COV_EXCLUDE) --html --output-dir coverage-report-integration
	@echo "Integration coverage: coverage-report-integration/html/index.html"

e2e-coverage: build $(E2E_CHILD)
	$(LLVM_COV_ENV) cargo llvm-cov --test e2e_tests $(COV_EXCLUDE) --html --output-dir coverage-report-e2e
	@echo "E2E coverage: coverage-report-e2e/html/index.html"

coverage: build $(E2E_CHILD)
	$(LLVM_COV_ENV) cargo llvm-cov --workspace --all-targets $(COV_EXCLUDE) --html --output-dir coverage-report
	@echo "Coverage: coverage-report/html/index.html"

# ── Clean ─────────────────────────────────────────────────────
clean:
	cargo clean
	rm -f $(E2E_CHILD)
	rm -rf coverage-report coverage-report-unit coverage-report-integration coverage-report-e2e
