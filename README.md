# piforge

A coding harness for Raspberry Pi 5 that closes the loop between your code and
the live sensor / GPIO / I2C state. It diagnoses and fixes the bugs that *look*
like broken hardware but are really numbering, address, library, or config
issues — and correctly triages the rest as hardware faults.

> The agent that **understands** live hardware state: typed sensor reads with
> units/ranges and sub-millisecond time-series correlation, instead of parsing
> ASCII from `i2cdetect` over SSH.

## Status

Early. The capability eval gate (`cmd/piforge-eval`) is the next milestone:
before betting on the local-appliance product, run the eval to verify a ~4B
model can actually diagnose hardware faults from live state (no public
benchmark covers this).

## Build

Pure Go (no CGo) — cross-compiles cleanly:

```
# native (e.g. on the Pi)
go build -o piforge ./cmd/piforge
go build -o piforge-eval ./cmd/piforge-eval

# cross-compile from macOS/Linux to Pi 5
GOOS=linux GOARCH=arm64 go build -o dist/piforge-arm64 ./cmd/piforge
GOOS=linux GOARCH=arm64 go build -o dist/piforge-eval-arm64 ./cmd/piforge-eval
```

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
cmd/piforge/        interactive agent
cmd/piforge-eval/   capability eval gate
internal/config/    config + defaults
internal/provider/  llama-server (OpenAI-compat) client w/ cache telemetry
internal/hil/       GPIO, I2C, scope, telemetry, code_edit, inventory tools
internal/broker/    safety gate (R/B/I classification, arming, under-voltage stop)
internal/agent/     ReAct loop with prefix-cache discipline
internal/eval/      case format + runner + scorer + decision rule
eval/cases/         ~40 case fixtures (the gate)
```

## Hardware notes

- Pi 5 GPIO is on the RP1 southbridge (`gpiochip4` by default; auto-discovered).
  Only `lgpio` works on Pi 5 — `RPi.GPIO`/`pigpio` are broken.
- Add your user to the `gpio`, `i2c`, `spi`, `dialout` groups for non-root access.
- The `vcgencmd get_throttled` under-voltage bit is a STOP signal before any
  load: brownouts corrupt the SD card.
- Max GPIO drive is 12 mA/pin on RP1 (not 16); no aggregate rail spec is published.
