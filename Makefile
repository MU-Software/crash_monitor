# crash_monitor — self-contained build / lint / test / coverage.
#
# Works standalone from a clone of just this repository. The end-to-end tests
# (which need a crash-producing child that links a host app's C reporter) are
# intentionally NOT here yet; they live in the embedding project's Makefile
# until a schema-only producer lands in tests/e2e/fixtures/.
#
# Common overrides:
#   make build SIGN_IDENTITY="Developer ID Application: ..."   # different signer
#   CRASH_MONITOR_DATA_DIR_NAME=.myapp make build             # bake a host default

.DEFAULT_GOAL := build

# Codesigning identity for the debugger entitlement (task_for_pid, vm_read).
# Without it the monitor cannot inspect a crashing child.
SIGN_IDENTITY ?= Apple Development
ENTITLEMENTS  := crash_monitor.entitlements

MONITOR_BIN        := target/release/crash_monitor
MONITOR_DIALOG_BIN := target/release/crash_dialog_macos

INTEGRATION_TESTS := --test shm_round_trip --test shm_validation_failure \
                     --test alarm_timeout --test event_loop_test

# Homebrew LLVM tools for cargo-llvm-cov (macOS).
LLVM_COV_ENV := LLVM_COV=/opt/homebrew/opt/llvm/bin/llvm-cov \
                LLVM_PROFDATA=/opt/homebrew/opt/llvm/bin/llvm-profdata
# Exclude untestable code from coverage:
#   platform/.*/ffi/ — platform FFI (macos/ffi/, future linux/ffi/)
#   main.rs          — OS orchestration (signal, spawn, waitpid)
#   paths.rs         — OS I/O (env::var, fs::create_dir_all)
#   platform/mod.rs  — FFI delegation wrappers
COV_EXCLUDE := --ignore-filename-regex '(platform/.*/ffi/|/main\.rs$$|/paths\.rs$$|platform/mod\.rs$$)'

.PHONY: build lint test unit-test integration-test \
        coverage unit-coverage integration-coverage clean

# ── Build (release + codesign) ────────────────────────────────
build:
	cargo build --release --workspace
	codesign --entitlements $(ENTITLEMENTS) --force --sign "$(SIGN_IDENTITY)" $(MONITOR_BIN)
	codesign --entitlements $(ENTITLEMENTS) --force --sign "$(SIGN_IDENTITY)" $(MONITOR_DIALOG_BIN)

# ── Lint ──────────────────────────────────────────────────────
lint:
	cargo fmt -- --check
	cargo clippy -- -D warnings -A dead_code

# ── Tests (unit + integration; e2e is host-side for now) ──────
unit-test:
	cargo test --lib

integration-test:
	cargo test $(INTEGRATION_TESTS)

test: unit-test integration-test

# ── Coverage (HTML reports under coverage-report*/) ───────────
unit-coverage:
	$(LLVM_COV_ENV) cargo llvm-cov --lib $(COV_EXCLUDE) --html --output-dir coverage-report-unit
	@echo "Unit coverage: coverage-report-unit/html/index.html"

integration-coverage:
	$(LLVM_COV_ENV) cargo llvm-cov $(INTEGRATION_TESTS) $(COV_EXCLUDE) --html --output-dir coverage-report-integration
	@echo "Integration coverage: coverage-report-integration/html/index.html"

coverage:
	$(LLVM_COV_ENV) cargo llvm-cov --lib $(INTEGRATION_TESTS) $(COV_EXCLUDE) --html --output-dir coverage-report
	@echo "Coverage: coverage-report/html/index.html"

# ── Clean ─────────────────────────────────────────────────────
clean:
	cargo clean
	rm -rf coverage-report coverage-report-unit coverage-report-integration coverage-report-e2e
