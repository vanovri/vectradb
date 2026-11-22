SEEDS ?= 5000

.PHONY: check test miri slt fuzz-oracle torture-spill tpch-bench wasm-studio verify-all

check:
	cargo fmt --all -- --check
	cargo clippy --workspace --all-targets -- -D warnings

test:
	cargo test --workspace --release

miri:
	cargo +nightly miri test -p vectra-types

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
	cd studio && npm run build && npm test

verify-all: check test slt fuzz-oracle torture-spill tpch-bench wasm-studio
