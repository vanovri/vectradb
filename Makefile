SEEDS ?= 5000
MIRI_TOOLCHAIN ?= nightly
TELEMETRY_SCRIPT := scripts/emit_telemetry_summary.mjs
TELEMETRY_OUT    := studio/public/telemetry/summary.json
TELEMETRY_LOGDIR := .telemetry-logs
TPCH_REPORT      := docs/benchmarks/tpch_results.md
PROGRESS_LEDGER  := PROGRESS.md
VECTRA_WASM_PATH ?=

.PHONY: check test miri slt fuzz-oracle torture-spill tpch-bench wasm-studio verify-all telemetry-summary verify-live

check:
	cargo fmt --all -- --check
	cargo clippy --workspace --all-targets -- -D warnings

test:
	cargo test --workspace --release

miri:
	cargo +$(MIRI_TOOLCHAIN) miri test -p vectra-types

slt:
	cargo test -p vectra-oracle --release --test slt_suite -- --nocapture

fuzz-oracle:
	cargo run --release -p vectra-oracle --bin vectra-sqlsmith -- --seeds $(SEEDS)

torture-spill:
	cargo test -p vectra-exec --release --test spill_torture -- --nocapture

tpch-bench:
	cargo run --release -p vectra-tpch --bin vectra-tpch-bench -- --sf 0.1 --validate-sf 0.01 --output docs/benchmarks/tpch_results.md

wasm-studio:
	cargo build --release -p vectra-wasm --target wasm32-unknown-unknown
	wasm-bindgen target/wasm32-unknown-unknown/release/vectra_wasm.wasm --out-dir studio/src/wasm --target web
	cd studio && npm run build && npm test

verify-all: check test slt fuzz-oracle torture-spill tpch-bench wasm-studio

telemetry-summary:
	@echo "==> Emitting VectraDB VERIFIED_LIVE telemetry summary (camelCase, /1024 KB)..."
	@node $(TELEMETRY_SCRIPT) \
		--report $(TPCH_REPORT) \
		--progress $(PROGRESS_LEDGER) \
		--out $(TELEMETRY_OUT) \
		$(if $(VECTRA_WASM_PATH),--wasm $(VECTRA_WASM_PATH),) \
		--strict

verify-live: verify-all telemetry-summary
	@echo "==> Verifying summary.json parity (--verify --strict)..."
	@node $(TELEMETRY_SCRIPT) \
		--report $(TPCH_REPORT) \
		--progress $(PROGRESS_LEDGER) \
		--out $(TELEMETRY_OUT) \
		$(if $(VECTRA_WASM_PATH),--wasm $(VECTRA_WASM_PATH),) \
		--verify --strict
	@echo "==> Running negative strict contract test (--report Cargo.toml --strict)..."
	@mkdir -p $(TELEMETRY_LOGDIR)
	@rm -f $(TELEMETRY_LOGDIR)/negative-summary.json
	@if node $(TELEMETRY_SCRIPT) \
		--report Cargo.toml \
		--progress $(PROGRESS_LEDGER) \
		--out $(TELEMETRY_LOGDIR)/negative-summary.json \
		$(if $(VECTRA_WASM_PATH),--wasm $(VECTRA_WASM_PATH),) \
		--strict > $(TELEMETRY_LOGDIR)/negative-test.log 2>&1; then \
		echo "ERROR: Negative strict test unexpectedly exited 0"; \
		exit 1; \
	fi
	@if [ -e "$(TELEMETRY_LOGDIR)/negative-summary.json" ]; then \
		echo "ERROR: Negative strict test created output file"; \
		exit 1; \
	fi
	@echo "==> Negative strict test PASSED (EXIT 1, no output file created)."
	@echo "==> VectraDB VERIFIED_LIVE gate PASSED."
