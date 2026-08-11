# Phase 0 — cactus-serve API probe (GO/NO-GO)

**Date:** 2026-08-11 · **Host:** ssnk@hallpi (Raspberry Pi 5 Model B Rev 1.0, 8GB)
**Cactus:** built from source (`github.com/cactus-compute/cactus`, `cactus build --python`)
**Model:** Qwen3-1.7B, CQ4, **rebuilt at `--cache-context-length 4096`** (see below)

## Verdict: GO (with two documented caveats that are resolved by config)

The four wire-shape unknowns resolve as follows. The agent loop's tool-calling
path works end-to-end against `cactus serve`.

| # | Unknown | Result | Evidence |
|---|---------|--------|----------|
| a | mounts `/v1/models` + `/v1/chat/completions` | **GO** | `GET /v1/models` → 200 `{"object":"list","data":[{"id":"qwen3-1.7b-cq4-ctx4096",...}]}` |
| b | accepts `tool_choice:"auto"` | **GO** | tool-call request with `tool_choice:"auto"` returned `finish_reason:"tool_calls"` |
| c | `tool_calls[].function.arguments` as JSON **string** | **GO** | `"arguments":"{\"action\": \"scan\"}"` (string) — exactly what `provider.rs:163` deserializes; **no adapter needed** |
| d | `usage.prompt_tokens_details.cached_tokens` present | **ABSENT (handled)** | cactus returns `{"prompt_tokens":187,"completion_tokens":20,"total_tokens":207}` with no `prompt_tokens_details`; `provider.rs` already defaults it to 0 via `#[serde(default)] Option<PromptTokensDetails>` |

### Caveat 1 — cactus validates the `model` name (RESOLVED by config)
Unlike llama-server (which ignores the `model` field), `cactus serve` rejects
unknown model names with HTTP 404: `Model 'piforge' is not available`. **Fix
applied:** `ServerConfig.model` is now configurable (default `"piforge"` for
llama-server back-compat); piforge.toml on the Pi sets `model =
"qwen3-1.7b-cq4-ctx4096"`. This is the config knob the plan's Design Analysis
sanctioned ("either a config knob ... or a one-line path prefix change").

### Caveat 2 — the default bundle's prefill context capacity is 128 tokens (RESOLVED by rebuild)
`cactus download` produces a bundle whose `decoder_full_context` graph is
compiled with a 128-token input buffer (the default in
`pad_no_cache_full_context_input`). Any prompt longer than ~128 tokens after
chat-template expansion — which includes every tool-calling request — fails
server-side: `[ERROR] [complete] Exception: context exceeds graph-bundle
full-context capacity` → HTTP 500. `cactus download` exposes no context flag,
but `cactus convert` does (`--cache-context-length`). **Fix applied:** rebuilt
the bundle via `cactus convert Qwen/Qwen3-1.7B --bits 4 --cache-context-length
4096`, which transpiles chunked prefill components sized for a 4096-token
context. The tool-call probe then succeeded. (The plan's RAM-headroom note
anticipated the 8192→4096 fallback; 4096 is what the Pi 5 8GB holds stably.)

## Captured request/response (the decisive tool-call probe)
```
POST /v1/chat/completions
{"model":"qwen3-1.7b-cq4-ctx4096",
 "messages":[{"role":"system","content":"You are a diagnostic agent. Always use tools when asked."},
             {"role":"user","content":"Scan the I2C bus now. Call the i2c tool with action=scan."}],
 "tools":[{"type":"function","function":{"name":"i2c","description":"I2C scan",
   "parameters":{"type":"object","properties":{"action":{"type":"string","enum":["scan"]}},"required":["action"]}}}],
 "tool_choice":"auto","max_tokens":100}

→ 200
{"choices":[{"message":{"role":"assistant","content":null,
   "tool_calls":[{"id":"call_01fc10f1d5bc468fbff90b27","type":"function",
     "function":{"name":"i2c","arguments":"{\"action\": \"scan\"}"}}]},
   "finish_reason":"tool_calls"}],
 "usage":{"prompt_tokens":187,"completion_tokens":20,"total_tokens":207}}
```
`arguments` is a JSON **string** (`"{\"action\": \"scan\"}"`) — provider.rs's
`ToolCallFunction.arguments: String` deserializes it directly. No custom
deserializer was needed (the plan's one Phase-0 code-change contingency is
unused).

## Provisioning note (not a wire-shape issue)
Standing cactus up on a Pi 5 required two undocumented steps beyond
`source ./setup`:
1. `cactus build --python` — `setup` does NOT build `libcactus_engine.so`; every
   model command fails with "Cactus library not found" until this is run.
2. Patching two engine sources (`telemetry_impl.cpp`, `complete.cpp`) that use
   `std::setw`/`std::setfill` without `#include <iomanip>` — the Pi's GCC
   (12.x) is stricter than upstream's toolchain. A one-line include added to
   each; rebuilt clean. (Upstream cactus bug, not a piforge issue; recorded in
   provisioning-manifest.txt for reproducibility.)

Both are cactus-side issues, recorded for reproducibility; neither changes
piforge code beyond the `model` config knob above.
