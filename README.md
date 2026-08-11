# piforge

A coding harness for Raspberry Pi 5 that closes the loop between your code and
the live sensor / GPIO / I2C state. It diagnoses and fixes the bugs that *look*
like broken hardware but are really numbering, address, library, or config
issues — and correctly triages the rest as hardware faults.

> The agent that **understands** live hardware state: typed sensor reads with
> units/ranges and sub-millisecond time-series correlation, instead of parsing
> ASCII from `i2cdetect` over SSH.

## Implementation

**Rust** (primary) lives under `rust/`. Pure Rust, no libgpiod C dependency
(uses the `gpiod` crate, which talks to `/dev/gpiochipN` directly). Cross-
compiles to a ~2.7 MB static `aarch64` binary you drop on the Pi.

A **Go** implementation under `internal/` + `cmd/` is preserved in the repo for
reference. Both are functional and red-team-tested; Rust is the active path.
See [`bench/RUST_VS_GO.md`](bench/RUST_VS_GO.md) for the measurement comparison.

## Status

Early. The capability eval gate (`piforge-eval`) is the next milestone: before
betting on the local-appliance product, run the eval to verify a ~4B model can
actually diagnose hardware faults from live state (no public benchmark covers
this).

## Build (Rust, primary)

```
# native (e.g. on the Pi 5)
cd rust && cargo build --release --features hw --bin piforge
cd rust && cargo build --release --bin piforge-eval

# cross-compile from macOS/Linux to the Pi 5 (needs aarch64-linux-gnu-gcc)
cargo build --release --features hw --target aarch64-unknown-linux-gnu --bin piforge
```

The `--features hw` flag compiles the real hardware tools (GPIO/I2C/scope/
telemetry via `gpiod` + `i2c-tools` + `vcgencmd`). Without it, the sim-backed
tools stand in so the loop runs on any host for dev/eval.

## Run

1. Start a local llama-server (OpenAI-compatible):

```
llama-server -m qwen3-4b-instruct-2507-q4_k_m.gguf \
  --jinja --port 8080 -c 8192 -ctk q8_0 -ctv q8_0
```

2. Run the agent:

```
./piforge --config piforge.toml --task "my BME280 keeps returning NaN for humidity"
```

GPIO outputs and I2C writes are Class I (physical) operations and require
per-action human approval by default. Reads (`i2c`, `gpio get`, `scope`,
`telemetry`) are always safe.

## Layout

```
rust/                        Rust implementation (primary)
  src/{config,provider,hil,broker,agent,sim,eval}.rs
  src/hil_hw.rs              real HW tools (Linux, behind the `hw` feature)
  src/bin/{piforge,piforge_eval}.rs
  tests/                     red-team regression tests
  benches/jitter.rs          scope-jitter micro-bench (the moat metric)
internal/ + cmd/             Go implementation (reference, preserved)
eval/cases/                  case fixtures covering the recurring bug archetypes
bench/                       baseline + comparison docs
docs/                        GitHub Pages site
.github/workflows/           ci.yml (Go) + rust.yml (Rust)
```

## Hardware notes

- Pi 5 GPIO is on the RP1 southbridge (`gpiochip4` by default; auto-discovered).
  Only `lgpio` works on Pi 5 — `RPi.GPIO`/`pigpio` are broken.
- Add your user to the `gpio`, `i2c`, `spi`, `dialout` groups for non-root access.
- The `vcgencmd get_throttled` under-voltage bit is a STOP signal before any
  load: brownouts corrupt the SD card.
- Max GPIO drive is 12 mA/pin on RP1 (not 16); no aggregate rail spec is published.

## License

Apache-2.0.
