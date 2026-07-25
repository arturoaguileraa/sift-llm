# Sift (sift-llm)

**A local PII gateway for AI coding agents.** Sift sits between your agent (opencode,
and any OpenAI/Anthropic-compatible tool) and the model API, and strips sensitive
data from your prompts before they ever leave your machine.

> Status: work in progress. Regex detection, a policy engine (shadow/enforce),
> multi-provider routing with model discovery, and **reversible pseudonymization with
> response rehydration** (buffered and streaming, including tool-call arguments) work
> today. Next: semantic (NER) detection and native Anthropic support. See
> [Roadmap](#roadmap).

![Sift demo](docs/demo.gif)

<!--
  Recorded from the real `sift` binary: serve, models, the live /v1/models
  endpoint, and enforce-mode redaction. Regenerate (needs vhs +
  zsh-syntax-highlighting):
  cargo build && vhs docs/demo.tape
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

## How it works

Sift is a local reverse proxy with two directions. **Outbound**, it detects sensitive
data, applies your policy, and swaps each value for a coherent, stable token
(`[EMAIL_1]`) before the request leaves your machine. **Inbound**, it rehydrates those
tokens back to the real values in the response — buffered or streamed — so your agent
sees usable output while the provider never sees your PII. A **vault** holds the
`token ⇄ value` mapping; it is scoped to a single request, because rehydrating the
response means your agent always resends real values on the next turn, so no persistent
session store is needed.

![Sift-LLM architecture](docs/architecture.svg)

For the longer story on why "cloud vs local" is a false trade-off, how detection works,
and where a small local model fits, read [**Why Sift-LLM**](docs/why-sift-llm.md).

## Requirements

- [Rust](https://www.rust-lang.org/tools/install) 1.75+ (`rustup` recommended)
- An API key for the provider you want to use behind Sift

## Install

### Binary (recommended)

Install the pre-compiled binary for macOS or Linux with a single command:

```bash
curl -fsSL https://raw.githubusercontent.com/arturoaguileraa/sift-llm/main/install.sh | bash
```

If the install directory isn't already on your `PATH`, the installer appends a small
delimited block to your shell rc (so `sift uninstall` can remove it cleanly later).

The installer verifies the download's **SHA-256 checksum** against the `.sha256`
published alongside each release before installing anything, and aborts on mismatch.
If the requested version is already installed it exits early; pass `--force` to
reinstall over it:

```bash
curl -fsSL https://raw.githubusercontent.com/arturoaguileraa/sift-llm/main/install.sh | bash -s -- --force
```

### From source

```bash
git clone https://github.com/arturoaguileraa/sift-llm.git
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

Add `-d` / `--daemon` to run it detached in the background (logs go to
`~/.config/sift/sift.log`); use `sift status` to check it and `sift stop` to stop it.
If the port is taken, Sift tells you and suggests `--port`.

**3. Register your providers.** The registry starts **empty**: only the providers you
add are exposed, nothing is seeded. Run `sift provider add` for an arrow-key picker
(popular providers, plus a "Custom URL" option for any OpenAI-compatible endpoint), or
pass flags directly. Your real keys stay on this machine.

```bash
sift provider add                       # interactive picker
sift provider add --url https://api.groq.com/openai/v1   # custom endpoint
sift provider list                      # see what's registered
sift provider remove groq               # drop one
```

As you add a provider, Sift **discovers its models** from the provider's `/models`
endpoint and exposes them through its single `/v1` endpoint.

**4. Point OpenCode at Sift.** OpenCode does **not** auto-discover models from a custom
OpenAI-compatible provider, so Sift writes the model list into OpenCode's config for
you:

```bash
sift sync-opencode
```

This adds (or updates) a `sift-llm` provider in `~/.config/opencode/opencode.jsonc`
pointing at `http://localhost:8787/v1`, listing every model from your registry. It
only touches that provider block and leaves the rest of your OpenCode config intact.

`sift provider add` and `sift provider remove` run this sync **automatically**, so
your OpenCode model list stays in step with your registry. **Restart OpenCode** after
a sync to pick up the changes: the models then appear under the **Sift LLM** provider,
each tagged `(Sift secured)`.

> Prefer to wire it by hand? Add the provider yourself in `opencode.jsonc` and list
> the models under `"models"` (each id must match `GET /v1/models`):
>
> ```json
> {
>   "$schema": "https://opencode.ai/config.json",
>   "provider": {
>     "sift-llm": {
>       "npm": "@ai-sdk/openai-compatible",
>       "options": { "baseURL": "http://localhost:8787/v1" }
>     }
>   }
> }
> ```

That's it. Use OpenCode as usual. Sift intercepts every request, applies your policy
(pseudonymizing sensitive values), forwards it with your real key, and rehydrates the
tokens back to real values in the streamed response.

## Configuration

`policies.yaml` controls what gets protected and how:

```yaml
mode: shadow          # shadow (log only, no changes) | enforce (act)

policies:
  api_key:     pseudonymize   # reversible token, restored in the response
  password:    pseudonymize
  credit_card: pseudonymize
  email:       pseudonymize
  person_name: pseudonymize
  ip_address:  pass           # left untouched (often needed in code)

allowlist:
  - "example.com"
  - "127.0.0.1"
```

Each category maps to one **action**:

- `pseudonymize` — replace with a coherent token (`[EMAIL_1]`) and **restore it in the
  response**. Reversible; the model never sees the real value. This is the default.
- `redact` — replace with a fixed tag (`[EMAIL_REDACTED]`). Irreversible, never restored.
- `block` — reject the whole request before it reaches the model (returns a 400).
- `pass` — leave the value untouched (functional data you want the model to see).

Start in `shadow` mode to see what *would* happen without changing anything, then
switch to `enforce` once the policy fits your workflow.

## How it works

```
opencode ──► [ Sift :8787 ] ──► provider API
   ▲              │  detect → policy → tokenize → forward with real key
   └── response ◄─┘  rehydrate tokens ◄── (buffered or streamed)
```

Sift exposes an OpenAI-compatible `/v1` endpoint, so your agent talks to it exactly
as it would talk to the real API. Your provider key lives only in Sift's
environment, never in the agent's config.

## CLI

| Command | What it does |
|---|---|
| `sift serve --config policies.yaml` | Start the gateway (the proxy) on `localhost:8787`. This is the product. Add `-d` / `--daemon` to detach into the background; `--port` to change the port. |
| `sift stop` | Stop a background gateway started with `--daemon`. |
| `sift provider add` | Register an upstream provider. Arrow-key picker (popular providers + custom URL), or pass `--url` / `--key-env` / `--api-key` directly. Keys stay local. Re-syncs OpenCode. |
| `sift provider list` | Show registered providers. |
| `sift provider remove <name>` | Remove a registered provider. Re-syncs OpenCode. |
| `sift sync-opencode` | Write the registry's models into OpenCode's config (`sift-llm` provider). Runs automatically on add/remove; `--path` overrides the config location. |
| `sift models` | List every model exposed to the agent, each tagged `(Sift secured)`. |
| `sift status` | Check whether the gateway is running (and its PID). |
| `sift scan <file>` | One-off diagnostic: show what would be detected/redacted. Not the proxy, just a tool to test your policy. |
| `sift uninstall` | Remove Sift's config, the OpenCode provider entry, the PATH block and the binary. `--yes` skips the prompt; `--keep-binary` keeps the binary. |

The demo gif above shows `serve`, `models`, the live `/v1/models` endpoint, and
`scan` redacting a file. In normal use the proxy is invisible: you start `sift serve`
once, point your agent at it, and it protects every request automatically.

## Roadmap

- [x] Phase 1: regex redaction + streaming passthrough
- [x] Phase 2: policy engine (`pass` / `redact` / `block`, allowlist, shadow/enforce)
- [x] Phase 6: multi-provider registry + routing, model discovery, OpenCode sync
- [x] Phase 3: reversible pseudonymization — coherent tokens + response rehydration,
      buffered **and** streaming (tokens reassembled across delta/transport splits).
      Uses a per-request vault; a persistent session vault is only needed for
      stateful APIs and is deliberately out of the main path.
- [x] Phase 4: tool-call and tool-result handling — tool-call `arguments` are
      rehydrated in both buffered and streamed responses; tool-result content in the
      request is scanned like any other field by the recursive redactor.
- [ ] Phase 5: semantic detection (NER via ONNX)
- [ ] Native provider protocols (Gemini `generateContent`, Anthropic `/v1/messages`),
      not just the OpenAI-compatible `/v1/chat/completions` surface

## Limitations

- **OpenAI-compatible protocol only.** Sift speaks `/v1/chat/completions`, so it routes
  every provider through its OpenAI-compatible surface. This breaks provider features
  that live outside that schema — notably **Gemini 3 "thinking" models used with tools**,
  which require a `thought_signature` on function calls that the OpenAI schema cannot
  carry across turns (the native Gemini API handles it). Use OpenAI, Groq/Llama,
  Anthropic, or non-thinking Gemini models until native provider adapters land.
- **Rehydration is exact-match**, so if the model mangles a token (splits it oddly or
  changes its case) it may not be restored. The `[TIPO_N]` format is chosen because
  models tend to copy it verbatim.
- **A pseudonymized value containing a JSON metacharacter** (e.g. a quote inside a
  password) could break a streamed frame, since streamed rehydration is a text splice.
  Emails, names and IPs are unaffected; a per-field-aware pass would remove this caveat.
- **Images and other media pass through untouched.**
- **Tool execution happens inside the agent**, so a tool that makes its own network
  request bypasses Sift. Sift only sees traffic to the model.
- Regex detection catches structured secrets well but misses contextual PII. Semantic
  detection lands in Phase 5.

## Uninstall

```bash
sift uninstall          # asks for confirmation first
sift uninstall --yes    # no prompt
```

This stops any running daemon and removes everything Sift created: its config
directory (`~/.config/sift`), the `sift-llm` provider it wrote into your OpenCode
config, the PATH block the installer added to your shell rc, and the binary itself.
Use `--keep-binary` to remove only the config and integrations.

## License

TBD.
