# Sift (sift-llm)

**A local PII gateway for AI coding agents.** Sift sits between your agent (opencode,
and any OpenAI/Anthropic-compatible tool) and the model API, and strips sensitive
data from your prompts before they ever leave your machine.

> Status: early work in progress. Phase 1 (regex redaction + passthrough) is the
> current milestone. See [Roadmap](#roadmap).

![Sift demo](docs/demo.gif)

<!--
  Preview only: launch the gateway, add providers, then OpenCode lists every
  discovered model as "<id> PII secured by Sift".
  Generated from mock `sift`/`opencode` in docs/preview/, to be regenerated
  from the real binary once Phase 1 lands.
  Regenerate (needs vhs + gum + zsh-syntax-highlighting):
  cd docs/preview && vhs preview.tape
-->

---

## Why

When you paste code, logs, or config into an AI coding agent, real secrets and PII
often go with it: API keys, connection strings, emails, customer names. No file
transfer alert fires, no DLP rule triggers. Sift is a local reverse proxy that
inspects that traffic and redacts sensitive data **before it leaves localhost**,
while keeping the response useful.

- **Harness-agnostic.** Works with anything that lets you set a base URL.
- **Model-agnostic.** Point it at Anthropic, OpenAI, or a local model.
- **Local by default.** Your data and your real API keys stay on your machine.

## Requirements

- [Rust](https://www.rust-lang.org/tools/install) 1.75+ (`rustup` recommended)
- An API key for the provider you want to use behind Sift

## Install

### From source

```bash
git clone https://github.com/<you>/sift-llm.git
cd sift-llm
cargo build --release
# binary is at ./target/release/sift
```

### With cargo

```bash
cargo install --path .
```

## Quick start

**1. Configure your policy** (copy the example and edit):

```bash
cp policies.example.yaml policies.yaml
```

**2. Start the gateway** (`sift serve`) with your real provider key in the
environment. This is a long-running daemon: start it once and leave it running.

```bash
export ANTHROPIC_API_KEY=sk-ant-...
sift serve --config policies.yaml
# ✓ Sift listening on http://localhost:8787  [mode: shadow]
```

**3. Register your providers.** Run `sift provider add` for an interactive picker
(popular providers, plus a "Custom URL" option for any OpenAI-compatible endpoint),
or pass flags directly. Your real keys stay on this machine.

```bash
sift provider add                       # interactive picker
sift provider add --url https://api.groq.com/openai/v1   # custom endpoint
```

As you add providers, Sift **discovers their models** and exposes them all through
its single `/v1` endpoint.

**4. Point your favourite harness (e.g. OpenCode) at Sift.** Add *one* provider in
`opencode.json` with your gateway endpoint:

```json
{
  "$schema": "https://opencode.ai/config.json",
  "provider": {
    "sift": {
      "npm": "@ai-sdk/openai-compatible",
      "options": { "baseURL": "http://localhost:8787/v1" }
    }
  }
}
```

OpenCode then lists **every model discovered behind the gateway**, each shown as its
own id plus `PII secured by Sift`:

```
claude-sonnet-4-6      PII secured by Sift
claude-opus-4-8        PII secured by Sift
llama-3.3-70b          PII secured by Sift
llama-3.1-8b-instant   PII secured by Sift
```

That's it. Use opencode as usual. Sift intercepts every request, redacts what your
policy says, forwards it with your real key, and streams the response back.

## Configuration

`policies.yaml` controls what gets protected and how:

```yaml
mode: shadow          # shadow (log only, no changes) | enforce (act)

policies:
  api_key:     block          # never leaves, not even redacted
  password:    block
  credit_card: block
  email:       redact         # replaced with a placeholder
  person_name: redact
  ip_address:  pass           # left untouched (often needed in code)

allowlist:
  - "example.com"
  - "127.0.0.1"
```

Start in `shadow` mode to see what *would* be redacted without breaking anything,
then switch to `enforce` once the policy fits your workflow.

## How it works

```
opencode ──► [ Sift :8787 ] ──► provider API
   ▲              │  detect  → policy → redact
   └── response ◄─┘  forward with real key
```

Sift exposes an OpenAI-compatible `/v1` endpoint, so your agent talks to it exactly
as it would talk to the real API. Your provider key lives only in Sift's
environment, never in the agent's config.

## CLI

| Command | What it does |
|---|---|
| `sift serve --config policies.yaml` | Start the gateway (the proxy). Long-running daemon on `localhost:8787`. This is the product. |
| `sift provider add` | Register an upstream provider. Interactive picker (popular providers + custom URL), or pass `--url` / `--key-env` directly. Keys stay local. |
| `sift provider list` | Show registered providers. |
| `sift models` | List every model exposed to the agent, each tagged `(Sift secured)`. |
| `sift scan <file>` | One-off diagnostic: show what would be detected/redacted. Not the proxy, just a tool to test your policy. |

The demo gif above shows `serve` + `provider add` + `models`. The proxy itself is
invisible in normal use: you start `sift serve` once, point your agent at it, and it
protects every request automatically in the background.

## Roadmap

- [x] Phase 1: regex redaction + streaming passthrough (single provider)
- [ ] Phase 2: policy engine (`pass` / `redact` / `block`, allowlist, shadow mode)
- [ ] Phase 3: reversible pseudonymization (coherent tokens + response rehydration)
- [ ] Phase 4: tool-call and tool-result handling
- [ ] Phase 5: semantic detection (NER via ONNX)
- [ ] Phase 6: multi-provider routing (one endpoint, many models)

## Limitations

- **Images and other media pass through untouched** in the current phase.
- **Tool execution happens inside the agent**, so a tool that makes its own network
  request bypasses Sift. Sift only sees traffic to the model.
- Regex detection (current phase) catches structured secrets well but misses
  contextual PII. Semantic detection lands in Phase 5.

## License

TBD.
