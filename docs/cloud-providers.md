# Cloud providers

piforge's `provider::Client` speaks the OpenAI-compatible Chat Completions API,
so it points at **any** hosted API-key model — not just a local llama-server or
cactus. A provider is selected purely by three settings; there is **no
per-provider code path** (no adapters, no SDKs).

## Configure a provider

**Easiest:** pick a provider by name, add your key. The endpoint + model are
filled from a built-in registry (see the table below):

```toml
# piforge.toml
[server]
provider = "zai-coding"   # any name from the table (or set PIFORGE_PROVIDER)
api_key  = "dummy"        # leave dummy; the real key comes from the env
```

```sh
export PIFORGE_API_KEY=<your-key>     # env wins over the toml api_key
```

**Manual / custom gateway:** leave `provider` unset and set the endpoint yourself:

```toml
[server]
base_url = "https://my-gateway.example/v1"
model    = "glm-4.6"
```

Resolution order: an explicit `base_url` / `model` (toml or env) always wins;
otherwise a `provider` fills both from the registry; an unknown provider name is
a config error that lists the known ones. `PIFORGE_API_KEY` (env) **overrides**
`[server] api_key`. With the env var unset, the default `"dummy"` is kept — which
a local llama-server/cactus ignores, so the local path is unchanged. **Never
write a real key into `piforge.toml`** — it would be committed and leak.

## Known providers

| `provider` | `base_url` | default `model` | notes |
|---|---|---|---|
| `zai-coding` | `https://api.z.ai/api/coding/paas/v4` | `glm-4.6` | bills a **GLM Coding Plan** subscription |
| `zai`, `zai-paas` | `https://api.z.ai/api/paas/v4` | `glm-4.6` | z.ai pay-as-you-go API balance / resource packages |
| `openai` | `https://api.openai.com/v1` | `gpt-4o` | |
| `kimi` | `https://api.moonshot.cn/v1` | `moonshot-v1-32k` | |
| `openrouter` | `https://openrouter.ai/api/v1` | `anthropic/claude-3.5-sonnet` | gateway → Claude / Gemini |

For a **self-hosted** OpenAI-compatible gateway (LiteLLM, vLLM, …), leave
`provider` unset and point `server.base_url` at it manually.

### z.ai: Coding Plan vs pay-as-you-go (pick the endpoint that matches your billing)

z.ai exposes **two OpenAI-compatible endpoints** that share one key but bill
different things — this is the most common z.ai gotcha:

- **`/api/paas/v4`** — the pay-as-you-go API. Bills your API balance / resource
  packages. Use this if you recharged API credit.
- **`/api/coding/paas/v4`** — the **GLM Coding Plan** endpoint. Bills your Coding
  Plan subscription (e.g. "GLM Coding Pro"). Use this if you have a Coding Plan;
  a Coding-Plan key on `/api/paas/v4` fails with `1113 Insufficient balance`.
- (z.ai also offers an **Anthropic-Messages** endpoint at `/api/anthropic` for the
  same Coding Plan — piforge doesn't need it; the OpenAI endpoint above works.)

Both speak the same OpenAI Chat Completions protocol piforge already uses, so
this is purely a `server.base_url` choice. `glm-4.6`/`glm-4.7` are **thinking
models** (they emit a `reasoning_content` field); piforge uses the `content` /
`tool_calls` and ignores it.

### Claude and Gemini — via a gateway, not native

Anthropic's Messages API and Google's Generative API are **not**
OpenAI-compatible, so piforge reaches Claude/Gemini through an OpenAI-compatible
gateway (OpenRouter, LiteLLM, etc.). piforge has no native Anthropic/Gemini
adapter — the one generic client covers every provider above. If you need a
native adapter, that is a separate, larger change.

## Auth, retry, and errors

- **Health check** (`GET /models`) is authenticated with the same Bearer token
  as chat completions — cloud `/v1/models` endpoints require it. (A local
  llama-server ignores the header.)
- **Transient failures are retried**: HTTP `429` and `5xx`, plus transient
  transport errors (request timeout, connection reset, broken pipe), up to 3
  retries with exponential backoff (1s/2s/4s + jitter, capped at 30s per sleep).
  Total added wait is ≤ ~15s worst case; a down provider fails a case after the
  retries exhaust rather than hanging.
- **Fail-fast on client errors**: `400/401/403/404` are not retried. `401/403`
  return a message naming `PIFORGE_API_KEY` + `base_url`, so a wrong or missing
  key is obvious instead of an opaque "HTTP 401".
- **No key leakage**: error messages and logs reference the env-var *name*
  (`PIFORGE_API_KEY`), never the value.

## Privacy

This is a real change from the local-appliance framing: pointing piforge at a
cloud provider sends the symptom, the setup file contents, and the tool results
(sensor reads, `dmesg`, board strings) **off the device** to the provider. The
local path keeps everything on the Pi; cloud is opt-in per the table above. Use
only a provider you trust with that data.

## Verifying a cloud model with the eval gate

Before betting on a cloud model, run the capability eval gate against it
(`bench/REAL_EVAL_cloud_<date>.md` follows the honesty pattern with 3 trust
links: the mock gate green, fixture consistency green, and the
non-converged/total call). The workflow mirrors the cactus probe:

1. **Probe the wire shape** (GO/NO-GO per unknown): an authenticated
   `GET /v1/models`, then a `POST /v1/chat/completions` with piforge's tool
   shape + `tool_choice: "auto"` + the model id. Confirm (a) `/v1/models`
   returns 200, (b) `tool_choice: "auto"` is accepted, (c)
   `tool_calls[].function.arguments` is a JSON **string**, and (d) a tool call is
   actually emitted (the model dispatches, not just text). Record it in
   `bench/cloud-probe-<provider>.md`; config-fix any divergence (e.g. a model id
   that rejects `tool_choice`).
2. **Run the gate**: with the cloud `piforge.toml` + `PIFORGE_API_KEY`, run the
   mock precondition first (`piforge-eval --mock` must pass every case and
   decide `BUILD_LOCAL` — a non-zero exit is a harness regression), then the
   real run (`piforge-eval` with no `--mock`; pass `--trace-dir` to keep a
   per-case JSONL record for the verdict). Capture the summary + per-case
   verdicts in `bench/REAL_EVAL_cloud_<date>.md`.

The verdict reflects the cloud model's real capability under the unchanged
scoring rules — a capable model is expected to `BUILD_LOCAL`; if it doesn't,
that is the honest finding.
