# piforge

A hardware-fault-clearance harness for Raspberry Pi 5. It distinguishes faults
that *look* like broken hardware (wrong BCM vs BOARD pin, RPi.GPIO on Pi 5,
software PWM jitter, wrong I2C address, BMP vs BME variant, missing overlay,
IIO scale, unit conversion) from **STOP** hardware faults (shorted I2C,
undervoltage). It may edit workspace-relative driver/config files when the gold
fix is software.

> The agent that **understands** live hardware state: typed sensor reads with
> units/ranges and sub-millisecond time-series correlation, instead of parsing
> ASCII from `i2cdetect` over SSH.

## Implementation

Rust lives under `rust/`. Pure Rust, no libgpiod C dependency (uses the `gpiod`
crate, which talks to `/dev/gpiochipN` directly). Cross-compiles to a ~2.7 MB
`aarch64` binary you drop on the Pi.

See [`bench/BASELINE.md`](bench/BASELINE.md) and
[`bench/RUST_VS_GO.md`](bench/RUST_VS_GO.md) for binary size, RSS, and
scope-jitter measurements.

## Status

Early. The capability eval gate (`piforge-eval`) is the next milestone: before
betting on the local-appliance product, run the eval to verify a ~4B model can
actually diagnose hardware faults from live state (no public benchmark covers
this).

## Build

```
# native (e.g. on the Pi 5)
cd rust && cargo build --release --features hw --bin piforge
cd rust && cargo build --release --bin piforge-eval

# cross-compile from macOS/Linux to the Pi 5 (needs aarch64-linux-gnu-gcc)
cargo build --release --features hw --target aarch64-unknown-linux-gnu --bin piforge
```

The `--features hw` flag compiles the real hardware tools (GPIO/I2C/scope/
telemetry via `gpiod` + in-process I2C ioctls + `vcgencmd`). Without it, the
sim-backed tools stand in so the loop runs on any host for dev/eval.

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

## Cloud providers

piforge speaks the OpenAI-compatible API, so it also points at any hosted,
API-key model — not just a local llama-server. **Pick a provider by name and add
your key**; the endpoint follows from the provider:

```
# in piforge.toml — choose a provider; the endpoint + model are filled in:
[server]
provider = "zai-coding"     # any name from the table below (or set PIFORGE_PROVIDER)

# the key comes from the environment, never the repo:
export PIFORGE_API_KEY=<your-key>
```

| `provider` | endpoint | model | notes |
|---|---|---|---|
| `zai-coding` | `https://api.z.ai/api/coding/paas/v4` | `glm-4.6` | bills a **GLM Coding Plan** subscription |
| `zai` / `zai-paas` | `https://api.z.ai/api/paas/v4` | `glm-4.6` | z.ai pay-as-you-go API balance (not the Coding Plan) |
| `openai` | `https://api.openai.com/v1` | `gpt-4o` | |
| `kimi` | `https://api.moonshot.cn/v1` | `moonshot-v1-32k` | |
| `openrouter` | `https://openrouter.ai/api/v1` | `anthropic/claude-3.5-sonnet` | gateway → Claude / Gemini |

Prefer the explicit path instead? Leave `provider` unset and set `server.base_url`
+ `server.model` yourself (or via `PIFORGE_BASE_URL`). The provider shortcut only
fills defaults — an explicit `base_url`/`model` (toml or env) always wins, and an
unknown provider name is a config error.

`PIFORGE_API_KEY` overrides `[server] api_key`; with it unset, the default
`"dummy"` keeps the local llama-server path working. Auth failures (401/403)
surface a message that names `PIFORGE_API_KEY`, and transient failures (429,
5xx, connection resets/timeouts) are retried with bounded backoff.

> **Privacy shift.** Pointing piforge at a cloud provider sends the symptom, the
> setup file contents, and the tool results (sensor reads, `dmesg`, board
> strings) **off the device** to the provider. The default local-appliance path
> keeps everything on the Pi; cloud is opt-in. Only use a provider you trust with
> that data.

**Claude / Gemini** are reached through an OpenAI-compatible gateway such as
OpenRouter or LiteLLM — piforge has no native Anthropic/Gemini adapter (the
generic client covers them). See [`docs/cloud-providers.md`](docs/cloud-providers.md)
for the full provider list, the retry/auth behavior, and the eval-gate workflow.

## Layout

```
rust/                        implementation
  src/{config,provider,hil,broker,agent,sim,eval}.rs
  src/hil_hw.rs              real HW tools (Linux, behind the `hw` feature)
  src/bin/{piforge,piforge_eval}.rs
  tests/                     red-team regression tests
  benches/jitter.rs          scope-jitter micro-bench (the moat metric)
eval/cases/                  case fixtures covering the recurring bug archetypes
bench/                       size / RSS / jitter notes
docs/                        GitHub Pages site
.github/workflows/           ci.yml (Rust)
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
