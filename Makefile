.PHONY: build build-arm64 test fmt clean eval-mock

# Native build. Eval binary always; hw `piforge` on Linux.
build:
	cd rust && cargo build --release --bin piforge-eval
ifeq ($(shell uname -s),Linux)
	cd rust && cargo build --release --features hw --bin piforge
endif

# Cross-compile the hw binary for Raspberry Pi 5 (aarch64).
build-arm64:
	cd rust && CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
		cargo build --release --features hw --target aarch64-unknown-linux-gnu --bin piforge

test:
	cd rust && cargo test

fmt:
	cd rust && cargo fmt

# Headless eval: scripted mock turns, example config, fixture cases.
eval-mock:
	cd rust && cargo build --release --bin piforge-eval
	./rust/target/release/piforge-eval --mock --config piforge.toml.example --cases eval/cases

clean:
	rm -rf dist/
