# Himalaya Code Usage

This guide covers the current Rust workspace under `rust/` and the `Himalaya` CLI binary. If you are brand new, make the doctor health check your first run: start `Himalaya`, then run `/doctor`.

## Quick-start health check

Run this before prompts, sessions, or automation:

```bash
cd rust
cargo build --workspace
./target/debug/Himalaya
# first command inside the REPL
/doctor
```

`/doctor` is the built-in setup and preflight diagnostic. Once you have a saved session, you can rerun it with `./target/debug/Himalaya --resume latest /doctor`.

## Prerequisites

- Rust toolchain with `cargo`
- One of:
  - `ANTHROPIC_API_KEY` for direct API access
  - `Himalaya login` for OAuth-based auth
- Optional: `ANTHROPIC_BASE_URL` when targeting a proxy or local service

## Install / build the workspace

```bash
cd rust
cargo build --workspace
```

The CLI binary is available at `rust/target/debug/Himalaya` after a debug build. Make the doctor check above your first post-build step.

## Quick start

### First-run doctor check

```bash
cd rust
./target/debug/Himalaya
/doctor
```

### Interactive REPL

```bash
cd rust
./target/debug/Himalaya
```

### One-shot prompt

```bash
cd rust
./target/debug/Himalaya prompt "summarize this repository"
```

### Shorthand prompt mode

```bash
cd rust
./target/debug/Himalaya "explain rust/crates/runtime/src/lib.rs"
```

### JSON output for scripting

```bash
cd rust
./target/debug/Himalaya --output-format json prompt "status"
```

## Model and permission controls

```bash
cd rust
./target/debug/Himalaya --model sonnet prompt "review this diff"
./target/debug/Himalaya --permission-mode read-only prompt "summarize Cargo.toml"
./target/debug/Himalaya --permission-mode workspace-write prompt "update README.md"
./target/debug/Himalaya --allowedTools read,glob "inspect the runtime crate"
```

Supported permission modes:

- `read-only`
- `workspace-write`
- `danger-full-access`

Model aliases currently supported by the CLI:

- `opus` → `Himalaya-opus-4-6`
- `sonnet` → `Himalaya-sonnet-4-6`
- `haiku` → `Himalaya-haiku-4-5-20251213`


## Adaptive model routing (`modelRouting`)

`Himalaya` can route different phases of a complex task to different models. The built-in balanced policy covers planning, coding, verification, and summarization; project settings can extend it with explicit routes.

Add project-local routing in `.Himalaya/settings.json`:

```json
{
  "modelRouting": {
    "enabled": true,
    "minFeedbackSamples": 2,
    "switchFailureThresholdPercent": 50,
    "routes": [
      {
        "phase": "verification",
        "model": "opus",
        "provider": "anthropic",
        "capabilities": ["verification", "test_generation"],
        "qualityWeight": 5,
        "latencyWeight": 2,
        "costWeight": 1,
        "maxTokens": 32000
      }
    ]
  }
}
```

Useful phases are `planning`, `coding`, `verification`, `summarization`, `vision`, and `local_fast`. Route weights are relative preferences: higher `qualityWeight` favors better models, higher `latencyWeight` penalizes slow routes more strongly, and higher `costWeight` penalizes expensive routes more strongly.

Adaptive routing uses persisted feedback to reduce confidence in routes with repeated failures, verification misses, recovery triggers, high latency, high token use, or high cost. Inspect the feedback loop with:

```bash
cd rust
./target/debug/Himalaya routes feedback summary
./target/debug/Himalaya --output-format json routes summary
```

The summary groups feedback by phase/model and includes success rate plus average latency, token, and cost metrics.

## Authentication

### API key

```bash
export ANTHROPIC_API_KEY="sk-ant-..."
```

### OAuth

```bash
cd rust
./target/debug/Himalaya login
./target/debug/Himalaya logout
```

### Which env var goes where

`Himalaya` accepts two Anthropic credential env vars and they are **not interchangeable** — the HTTP header Anthropic expects differs per credential shape. Putting the wrong value in the wrong slot is the most common 401 we see.

| Credential shape | Env var | HTTP header | Typical source |
|---|---|---|---|
| `sk-ant-*` API key | `ANTHROPIC_API_KEY` | `x-api-key: sk-ant-...` | [console.anthropic.com](https://console.anthropic.com) |
| OAuth access token (opaque) | `ANTHROPIC_AUTH_TOKEN` | `Authorization: Bearer ...` | `Himalaya login` or an Anthropic-compatible proxy that mints Bearer tokens |
| OpenRouter key (`sk-or-v1-*`) | `OPENAI_API_KEY` + `OPENAI_BASE_URL=https://openrouter.ai/api/v1` | `Authorization: Bearer ...` | [openrouter.ai/keys](https://openrouter.ai/keys) |

**Why this matters:** if you paste an `sk-ant-*` key into `ANTHROPIC_AUTH_TOKEN`, Anthropic's API will return `401 Invalid bearer token` because `sk-ant-*` keys are rejected over the Bearer header. The fix is a one-line env var swap — move the key to `ANTHROPIC_API_KEY`. Recent `Himalaya` builds detect this exact shape (401 + `sk-ant-*` in the Bearer slot) and append a hint to the error message pointing at the fix.

**If you meant a different provider:** if `Himalaya` reports missing Anthropic credentials but you already have `OPENAI_API_KEY`, `XAI_API_KEY`, or `DASHSCOPE_API_KEY` exported, you most likely forgot to prefix the model name with the provider's routing prefix. Use `--model openai/gpt-4.1-mini` (OpenAI-compat / OpenRouter / Ollama), `--model grok` (xAI), or `--model qwen-plus` (DashScope) and the prefix router will select the right backend regardless of the ambient credentials. The error message now includes a hint that names the detected env var.

**Namespaced model IDs:** if your model name already looks like `provider/model-name` but the provider is not one of the built-in Anthropic aliases, `Himalaya` now treats it as an OpenAI-compatible slug by default instead of falling back to Anthropic. That means OpenRouter-style names and other gateway slugs can work without extra prefixing, as long as `OPENAI_BASE_URL` and the matching API key are configured.

## Local Models

`Himalaya` can talk to local servers and provider gateways through either Anthropic-compatible or OpenAI-compatible endpoints. Use `ANTHROPIC_BASE_URL` with `ANTHROPIC_AUTH_TOKEN` for Anthropic-compatible services, or `OPENAI_BASE_URL` with `OPENAI_API_KEY` for OpenAI-compatible services. OAuth is Anthropic-only, so when `OPENAI_BASE_URL` is set you should use API-key style auth instead of `Himalaya login`.

### Anthropic-compatible endpoint

```bash
export ANTHROPIC_BASE_URL="http://127.0.0.1:8080"
export ANTHROPIC_AUTH_TOKEN="local-dev-token"

cd rust
./target/debug/Himalaya --model "Himalaya-sonnet-4-6" prompt "reply with the word ready"
```

### OpenAI-compatible endpoint

```bash
export OPENAI_BASE_URL="http://127.0.0.1:8000/v1"
export OPENAI_API_KEY="local-dev-token"

cd rust
./target/debug/Himalaya --model "qwen2.5-coder" prompt "reply with the word ready"
```

### Ollama

```bash
export OPENAI_BASE_URL="http://127.0.0.1:11434/v1"
unset OPENAI_API_KEY

cd rust
./target/debug/Himalaya --model "llama3.2" prompt "summarize this repository in one sentence"
```

### OpenRouter

```bash
export OPENAI_BASE_URL="https://openrouter.ai/api/v1"
export OPENAI_API_KEY="sk-or-v1-..."

cd rust
./target/debug/Himalaya --model "openai/gpt-4.1-mini" prompt "summarize this repository in one sentence"
```

### Alibaba DashScope (Qwen)

For Qwen models via Alibaba's native DashScope API (higher rate limits than OpenRouter):

```bash
export DASHSCOPE_API_KEY="sk-..."

cd rust
./target/debug/Himalaya --model "qwen/qwen-max" prompt "hello"
# or bare:
./target/debug/Himalaya --model "qwen-plus" prompt "hello"
```

Model names starting with `qwen/` or `qwen-` are automatically routed to the DashScope compatible-mode endpoint (`https://dashscope.aliyuncs.com/compatible-mode/v1`). You do **not** need to set `OPENAI_BASE_URL` or unset `ANTHROPIC_API_KEY` — the model prefix wins over the ambient credential sniffer.

Reasoning variants (`qwen-qwq-*`, `qwq-*`, `*-thinking`) automatically strip `temperature`/`top_p`/`frequency_penalty`/`presence_penalty` before the request hits the wire (these params are rejected by reasoning models).

## Supported Providers & Models

`Himalaya` has three built-in provider backends. The provider is selected automatically based on the model name, falling back to whichever credential is present in the environment.

### Provider matrix

| Provider | Protocol | Auth env var(s) | Base URL env var | Default base URL |
|---|---|---|---|---|
| **Anthropic** (direct) | Anthropic Messages API | `ANTHROPIC_API_KEY` or `ANTHROPIC_AUTH_TOKEN` or OAuth (`Himalaya login`) | `ANTHROPIC_BASE_URL` | `https://api.anthropic.com` |
| **xAI** | OpenAI-compatible | `XAI_API_KEY` | `XAI_BASE_URL` | `https://api.x.ai/v1` |
| **OpenAI-compatible** | OpenAI Chat Completions | `OPENAI_API_KEY` | `OPENAI_BASE_URL` | `https://api.openai.com/v1` |
| **DashScope** (Alibaba) | OpenAI-compatible | `DASHSCOPE_API_KEY` | `DASHSCOPE_BASE_URL` | `https://dashscope.aliyuncs.com/compatible-mode/v1` |

The OpenAI-compatible backend also serves as the gateway for **OpenRouter**, **Ollama**, and any other service that speaks the OpenAI `/v1/chat/completions` wire format — just point `OPENAI_BASE_URL` at the service.

**Model-name prefix routing:** If a model name starts with `openai/`, `gpt-`, `qwen/`, or `qwen-`, the provider is selected by the prefix regardless of which env vars are set. This prevents accidental misrouting to Anthropic when multiple credentials exist in the environment.

If a model name uses a namespace but is not one of the built-in Anthropic namespaces, the runtime now prefers the OpenAI-compatible provider. In practice, that means slugs such as `Mirage335/Nemotron-3-Nano-30B-A3B-virtuoso:latest` resolve through the OpenAI-compatible path instead of stopping at the Anthropic credential check.

### Tested models and aliases

These are the models registered in the built-in alias table with known token limits:

| Alias | Resolved model name | Provider | Max output tokens | Context window |
|---|---|---|---|---|
| `opus` | `Himalaya-opus-4-6` | Anthropic | 32 000 | 200 000 |
| `sonnet` | `Himalaya-sonnet-4-6` | Anthropic | 64 000 | 200 000 |
| `haiku` | `Himalaya-haiku-4-5-20251213` | Anthropic | 64 000 | 200 000 |
| `grok` / `grok-3` | `grok-3` | xAI | 64 000 | 131 072 |
| `grok-mini` / `grok-3-mini` | `grok-3-mini` | xAI | 64 000 | 131 072 |
| `grok-2` | `grok-2` | xAI | — | — |

Any model name that does not match an alias is passed through verbatim. This is how you use OpenRouter model slugs (`openai/gpt-4.1-mini`), Ollama tags (`llama3.2`), or full Anthropic model IDs (`Himalaya-sonnet-4-20250514`).

### User-defined aliases

You can add custom aliases in any settings file (`~/.Himalaya/settings.json`, `.Himalaya/settings.json`, or `.Himalaya/settings.local.json`):

```json
{
  "aliases": {
    "fast": "Himalaya-haiku-4-5-20251213",
    "smart": "Himalaya-opus-4-6",
    "cheap": "grok-3-mini"
  }
}
```

Local project settings override user-level settings. Aliases resolve through the built-in table, so `"fast": "haiku"` also works.

### How provider detection works

1. If the resolved model name starts with `Himalaya` → Anthropic.
2. If it starts with `grok` → xAI.
3. Otherwise, `Himalaya` checks which credential is set: `ANTHROPIC_API_KEY`/`ANTHROPIC_AUTH_TOKEN` first, then `OPENAI_API_KEY`, then `XAI_API_KEY`.
4. If nothing matches, it defaults to Anthropic.

## FAQ

### What about Codex?

The name "codex" appears in the Himalaya Code ecosystem but it does **not** refer to OpenAI Codex (the code-generation model). Here is what it means in this project:

- **`oh-my-codex` (OmX)** is the workflow and plugin layer that sits on top of `Himalaya`. It provides planning modes, parallel multi-agent execution, notification routing, and other automation features. See [PHILOSOPHY.md](./PHILOSOPHY.md) and the [oh-my-codex repo](https://github.com/Yeachan-Heo/oh-my-codex).
- **`.codex/` directories** (e.g. `.codex/skills`, `.codex/agents`, `.codex/commands`) are legacy lookup paths that `Himalaya` still scans alongside the primary `.Himalaya/` directories.
- **`CODEX_HOME`** is an optional environment variable that points to a custom root for user-level skill and command lookups.

`Himalaya` does **not** support OpenAI Codex sessions, the Codex CLI, or Codex session import/export. If you need to use OpenAI models (like GPT-4.1), configure the OpenAI-compatible provider as shown above in the [OpenAI-compatible endpoint](#openai-compatible-endpoint) and [OpenRouter](#openrouter) sections.

## HTTP proxy support

`Himalaya` honours the standard `HTTP_PROXY`, `HTTPS_PROXY`, and `NO_PROXY` environment variables (both upper- and lower-case spellings are accepted) when issuing outbound requests to Anthropic, OpenAI-, and xAI-compatible endpoints. Set them before launching the CLI and the underlying `reqwest` client will be configured automatically.

### Environment variables

```bash
export HTTPS_PROXY="http://proxy.corp.example:3128"
export HTTP_PROXY="http://proxy.corp.example:3128"
export NO_PROXY="localhost,127.0.0.1,.corp.example"

cd rust
./target/debug/Himalaya prompt "hello via the corporate proxy"
```

### Programmatic `proxy_url` config option

As an alternative to per-scheme environment variables, the `ProxyConfig` type exposes a `proxy_url` field that acts as a single catch-all proxy for both HTTP and HTTPS traffic. When `proxy_url` is set it takes precedence over the separate `http_proxy` and `https_proxy` fields.

```rust
use api::{build_http_client_with, ProxyConfig};

// From a single unified URL (config file, CLI flag, etc.)
let config = ProxyConfig::from_proxy_url("http://proxy.corp.example:3128");
let client = build_http_client_with(&config).expect("proxy client");

// Or set the field directly alongside NO_PROXY
let config = ProxyConfig {
    proxy_url: Some("http://proxy.corp.example:3128".to_string()),
    no_proxy: Some("localhost,127.0.0.1".to_string()),
    ..ProxyConfig::default()
};
let client = build_http_client_with(&config).expect("proxy client");
```

### Notes

- When both `HTTPS_PROXY` and `HTTP_PROXY` are set, the secure proxy applies to `https://` URLs and the plain proxy applies to `http://` URLs.
- `proxy_url` is a unified alternative: when set, it applies to both `http://` and `https://` destinations, overriding the per-scheme fields.
- `NO_PROXY` accepts a comma-separated list of host suffixes (for example `.corp.example`) and IP literals.
- Empty values are treated as unset, so leaving `HTTPS_PROXY=""` in your shell will not enable a proxy.
- If a proxy URL cannot be parsed, `Himalaya` falls back to a direct (no-proxy) client so existing workflows keep working; double-check the URL if you expected the request to be tunnelled.

## Common operational commands

```bash
cd rust
./target/debug/Himalaya status
./target/debug/Himalaya sandbox
./target/debug/Himalaya agents
./target/debug/Himalaya mcp
./target/debug/Himalaya skills
./target/debug/Himalaya system-prompt --cwd .. --date 2026-04-04
```


## Durable task scheduler daemon

The task scheduler can run one tick at a time for debugging or as a bounded daemon loop for long-running task execution.

```bash
cd rust

# Inspect queue and scheduler state
./target/debug/Himalaya tasks scheduler queue
./target/debug/Himalaya tasks scheduler status

# Run foreground scheduler ticks
./target/debug/Himalaya tasks scheduler run --once
./target/debug/Himalaya tasks scheduler run --max-ticks 5

# Start, inspect, and stop the daemon loop
./target/debug/Himalaya tasks daemon start --max-ticks 50
./target/debug/Himalaya tasks daemon status
./target/debug/Himalaya tasks daemon logs --limit 50
./target/debug/Himalaya tasks daemon stop
```

Use `daemon start --once` as a safe smoke test before allowing a longer daemon run. `daemon logs` emits the persisted daemon event stream, and `daemon status` reports the daemon state file and event log path in JSON/stream-json modes.

## Worker supervisor and isolated workers

Workers are durable local execution records that the scheduler and supervisor can inspect, restart, prompt, and complete. They can run directly in the current workspace or in a detached git worktree for safer parallel execution.

```bash
cd rust

# Create an idle worker record for the current workspace
./target/debug/Himalaya workers create --cwd ..

# Spawn a real process-backed worker
./target/debug/Himalaya workers spawn --cwd .. -- sh -c 'echo ready'

# Spawn with a detached git worktree rooted under a custom directory
./target/debug/Himalaya workers spawn --cwd .. --isolate-worktree --worktree-root ../.Himalaya-worker-trees -- sh -c 'pwd && git status --short'

# Inspect and advance lifecycle state
./target/debug/Himalaya workers list
./target/debug/Himalaya workers probe <worker-id>
./target/debug/Himalaya workers ready <worker-id>
./target/debug/Himalaya workers prompt <worker-id> "continue the assigned task"
./target/debug/Himalaya workers complete <worker-id> stop 1

# Let the supervisor reconcile stale/running/blocked workers
./target/debug/Himalaya workers supervise
```

`workers spawn --isolate-worktree` creates a detached worktree from `HEAD` and records `worker.isolation` metadata, including the source cwd and worktree path. The VS Code Task Board consumes worker stream events and can show process, isolation, restart, trust, prompt, and recent-event details for the selected worker.

## Benchmarking routing and orchestration quality

The built-in benchmark suite scores planner coverage, decomposition, scheduler readiness, MoE route coverage, adaptive routing quality, and execution harness progress.

```bash
cd rust
./target/debug/Himalaya benchmark list
./target/debug/Himalaya benchmark run --max-parallelism 4
./target/debug/Himalaya --output-format json benchmark run --record
```

Recorded benchmark runs are appended under `.Himalaya/benchmarks/runs.jsonl`. The JSON summary includes `average_adaptive_routing_quality_score`; each task result includes `score.adaptive_routing_quality_score` and the route decisions used for that task.

## Session management

REPL turns are persisted under `.Himalaya/sessions/` in the current workspace.

```bash
cd rust
./target/debug/Himalaya --resume latest
./target/debug/Himalaya --resume latest /status /diff
```

Useful interactive commands include `/help`, `/status`, `/cost`, `/config`, `/session`, `/model`, `/permissions`, and `/export`.

## Config file resolution order

Runtime config is loaded in this order, with later entries overriding earlier ones:

1. `~/.Himalaya.json`
2. `~/.config/Himalaya/settings.json`
3. `<repo>/.Himalaya.json`
4. `<repo>/.Himalaya/settings.json`
5. `<repo>/.Himalaya/settings.local.json`

## Mock parity harness

The workspace includes a deterministic Anthropic-compatible mock service and parity harness.

```bash
cd rust
./scripts/run_mock_parity_harness.sh
```

Manual mock service startup:

```bash
cd rust
cargo run -p mock-anthropic-service -- --bind 127.0.0.1:0
```

## Verification

```bash
cd rust
cargo test --workspace
```

## Workspace overview

Current Rust crates:

- `api`
- `commands`
- `compat-harness`
- `mock-anthropic-service`
- `plugins`
- `runtime`
- `rusty-Himalaya-cli`
- `telemetry`
- `tools`

## VS Code extension — 推理可视化 (Reasoning visualization)

The VS Code extension supports a "Reasoning visualization" feature that can display structured model reasoning steps (stream events with type `reasoning_step`) inside the chat panel. The visualization is disabled by default; toggle it from the chat panel top bar using the magnifier button (🔎). The chosen state is persisted per workspace (workspaceState key: `himalayaCode.showReasoning.v1`). When disabled, the extension will not forward reasoning events to the webview.
