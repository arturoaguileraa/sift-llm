# Sift-LLM: use the best cloud models without handing them your data

> How a small local proxy lets you have both the quality of a large model and the
> privacy of one running on your own machine.

## The dilemma that shows up the moment you plug an AI agent into your code

Coding agents (opencode, and in general any tool that talks to OpenAI or Anthropic)
are enormously useful. But they come with a side effect almost nobody talks about:
to help you, they read your code, your logs, and your configuration, and all of that
travels to a third party's server.

And it is not just "code" that goes. API keys, database connection strings, customer
emails, real names, tokens. No file-transfer alert fires. No DLP rule triggers. You
just paste it into the prompt, or the agent opens a file, and that data is already
off your machine.

## Cloud vs local: a false dilemma

The usual answer to this problem is framed as a choice between two evils:

- **Cloud model.** Top-tier quality and speed. In exchange, you send sensitive data
  to an external company and trust their retention policy.
- **Local model.** Your data never leaves home. In exchange, worse quality and
  speed, and expensive hardware just to get close to a large model.

It is presented as if you had to sacrifice one thing to get the other: either a good
model or privacy. Sift-LLM starts from the idea that this dilemma is false. **What
leaves your machine does not have to be the same thing the model processes.**

If you swap sensitive data for placeholders before the request leaves, and undo that
swap when the response comes back, you get both at once: the large cloud model works
on the structure of your problem, but never sees the real data. The quality is that
of the cloud model. The privacy is that of a local one.

## The idea: a local proxy that anonymizes and rehydrates

Sift is a reverse proxy that runs on your own machine. The agent thinks it is talking
directly to the model API, but two steps sit in between:

![Sift-LLM architecture](architecture.svg)

- **Outbound.** It detects sensitive data, applies your policy, and swaps each value
  for a stable token before the request crosses into the cloud. What travels is
  tokens, no PII.
- **Inbound.** When the response comes back with those tokens inside, it swaps them
  back to the real values before handing it to the agent.
- **Session vault.** A small in-memory vault holds the `token ⇄ value` mapping so the
  round trip is possible. Encrypted, alive only for the duration of the session,
  never written to disk.

The agent sees real, usable data. The model provider only ever sees tokens. Neither
the agent nor the provider knows there is anything in the middle.

## Step 1: detect what is sensitive

None of this works if you do not detect what is sensitive in the first place. The
first layer of detection is regular expressions and validators: patterns for API
keys, connection strings, emails, IPs, file paths, credit cards. It is fast,
deterministic, and covers most of the secrets that show up in a coding flow.

One detail that matters: in a coding agent, PII mostly does not come in through what
you type, but through what **the agent reads**. When it opens a `.env`, a config
file, or a dump, that is where the real secrets appear. That is why the detector
treats those read contents as the primary source, not as an afterthought.

## Step 2: decide what to do with each value (policies)

Detection alone is not enough: not everything should be treated the same way. Sift
uses a policy engine configurable per category, with three actions:

- **`pass`.** Let it through. A functional value, like a loopback IP, that the model
  needs in order to actually help you.
- **`pseudonymize`.** Swap it for a reversible token. Emails, names: things you want
  back in the response.
- **`block`.** Cut it irreversibly. Pure secrets like API keys or passwords: those
  should never come back, so they are not even stored for rehydration.

The rule of thumb is simple: **a secret you do not need back, block; personal data
you do need back, pseudonymize; functional data, pass.** And there is a `shadow` mode
that only audits without touching anything, so you can calibrate the rules before
letting them act and breaking something.

## The small model that is coming: semantic detection (NER)

Regular expressions have a ceiling. They see an email because it has an `@`, but they
do not see that "email Maria Gonzalez, the support lead" contains a person's name.
For that you need to understand language, not just match patterns.

The next piece (**not built yet**, it is on the roadmap) is a small NER model that
runs **inside the proxy itself**, on your machine, with no connection to any external
service. A compact named-entity-recognition model, executed in-process, that detects
names, addresses, and other entities the regex cannot catch.

The interesting part of the approach is the division of labor: the small local model
does the delicate part (finding and hiding what is sensitive) and the large cloud
model does the heavy part (reasoning about your problem). The large model never sees
the data; the small model never leaves your machine.

## The vault and token consistency

For rehydration to work, tokens have to be **consistent**. If `alice@company.com`
becomes `[EMAIL_1]`, it has to stay `[EMAIL_1]` for the whole session, not `[EMAIL_1]`
once and `[EMAIL_3]` the next time. Otherwise the model loses track of who you are
referring to and the multi-turn conversation breaks.

The vault is a bidirectional in-memory map that guarantees that consistency: before
creating a new token it checks whether the value already has one and reuses it. The
`[TYPE_N]` format with brackets is chosen on purpose so the model treats it as a
marker and copies it verbatim instead of rewriting it. And because it holds real PII,
it lives encrypted, dies with the session, is wiped from memory deterministically,
and never shows up in logs.

## Rehydration: the part that makes it feel natural

This is the difference between "redaction" (cross it out and done) and "reversible
anonymization". A proxy that only redacts hands you a response full of
`[EMAIL_REDACTED]`: safe, but awkward to read and use. Sift goes one step further and
**undoes** the swap in the response, so the experience is as if there had never been
a middleman.

There is some technical nuance here, because the response arrives streamed, chunk by
chunk. The rehydrator uses a sliding-window buffer: it accumulates fragments, swaps
the complete tokens, and holds back the tail that could be the start of a half-formed
token (for example `[EMA`) until the next chunk arrives. It also reaches into tool
call arguments: if the model responds `send_email(to="[EMAIL_1]")`, the real email
has to be restored before the agent runs that action, or it would send the mail to a
literal token.

The result, from the user's point of view, is that everything flows. You see real
names, real emails, useful answers. The only thing that changed is that the cloud
model, along the way, never got to see them.

## Where it is today and where it is going

Sift is being built in phases, and it is worth being honest about what works today
and what does not:

1. **One-way regex redaction (done).** Working proxy, pattern-based detection,
   irreversible redaction. Already useful on its own.
2. **Policy engine (done).** `pass` / `redact` / `block` per category, allowlist,
   and shadow/enforce modes.
3. **Reversible vault and rehydration (next).** The jump from "crossing out" to
   "anonymize and give back". This is where the streaming buffer comes in.
4. **Tools.** Rehydrate tool_call arguments and scan what the agent reads.
5. **Local NER.** The small model for semantic PII.
6. **Multi-provider.** A canonical adapter for Anthropic in addition to OpenAI.

The diagram above shows the full flow, which is the goal. Today Sift does the
detection and redaction part; reversible tokenization and rehydration are the next
milestone.

## Closing

The premise of Sift-LLM is that you should not have to choose between a good model and
your privacy. With a local proxy that anonymizes on the way out and rehydrates on the
way in, the sensitive data stays on your machine and the large model keeps doing its
job on the structure of the problem. Good quality and data at home, at the same time.

The project is open source and under active development:
[github.com/arturoaguileraa/sift-llm](https://github.com/arturoaguileraa/sift-llm).
