# Driving Claude Code / the Claude Agent SDK / the Anthropic API from Rust (2026)

**There is no official Anthropic Rust SDK.** The official Claude Agent SDK ships only as Python and TypeScript packages (`claude-agent-sdk` / `@anthropic-ai/claude-agent-sdk`), plus the `claude` CLI itself (which the Agent SDK docs now describe as the CLI surface of the same product). Anthropic's official Messages-API SDK list (Python, TypeScript, Go, Java, Ruby, C#, PHP) also has no Rust entry. Anything Rust-shaped here is either (a) driving the `claude` binary as a subprocess, (b) an unofficial, community-maintained HTTP client crate, or (c) meetrs exposing itself as an MCP server that Claude Code calls into — not Claude Code calling into meetrs' Rust code as a library.

## Recommendation

For meetrs, use the **headless `claude` CLI as a subprocess**, driven with `--print --output-format stream-json --input-format stream-json`, for the summarize/extract-action-items step. This gets you: the user's existing Claude Code subscription auth (no separate API key to manage), full tool use / MCP support, and a stable-ish JSON-lines wire format your Rust code parses with `serde_json`, all without depending on any unofficial crate whose maintenance you can't control. Pair this with the second half of the recommendation: build meetrs' own transcript/notes access as an **MCP server** (using the official `rmcp` crate) so Claude Code — or any other MCP-capable client — can pull meeting context on demand, instead of meetrs pushing a wall of transcript text into every prompt. If you outgrow the CLI's ergonomics (need fine-grained streaming control, prompt caching headers, or to run headless with a bare API key and no local `claude` install), fall back to raw HTTP with `reqwest` + `serde` against `/v1/messages` — it's more code, but it depends on nothing except Anthropic's stable REST contract. Avoid betting the core pipeline on any of the ~10 community Anthropic-client crates on crates.io; they are all thin, unofficially maintained, and none has reached 1.0 stability commensurate with sustained use.

## Comparison table

| Approach | Auth | Maturity/stability | Tool use / MCP | Effort to integrate | Best for |
|---|---|---|---|---|---|
| **`claude` CLI subprocess** (`-p --output-format stream-json`) | Reuses Claude Code subscription login (`~/.claude` credentials) *or* `ANTHROPIC_API_KEY` | Anthropic-maintained; JSON schema is documented but explicitly evolving (fields added across patch versions) | Full — built-in tools, `--mcp-config`, permission modes, hooks | Low-medium: spawn process, write to stdin, parse NDJSON from stdout | meetrs' "summarize + extract action items" step; reuses whatever auth the user already has for Claude Code |
| **Community Rust crate** (`misanthropy`, `clust`, `anthropic-ai-sdk`, `async-anthropic`, etc.) | API key only | Unofficial, thin wrappers, most single-maintainer, none at 1.0 with broad adoption | Usually basic tool-use passthrough; no MCP client support | Low (it's "just" a crate dependency) — but review the code, it's often a few hundred lines | Prototyping only; not recommended for a shipped product's core path |
| **Raw HTTP** (`reqwest` + `serde_json`, SSE by hand) | API key (or a Bearer token minted via `ant auth print-credentials --access-token` plus `anthropic-beta: oauth-2025-04-20`) | Rock solid — it's Anthropic's actual public contract | Full, if you implement the tool-use loop yourself; no MCP client, you'd have to speak MCP separately | Medium-high: SSE parsing, tool-use loop, cache headers, error handling all on you | Long-term primary integration once you know exactly what you need, or when you must run without a local `claude` binary |
| **meetrs as an MCP server** (`rmcp`) | N/A — meetrs is the callee, not the caller | `rmcp` is Anthropic-endorsed as the official Rust MCP SDK, actively maintained, huge adoption (see below) | This *is* MCP, by construction | Medium: define tools/resources, wire stdio or HTTP transport | Exposing transcripts/notes/action-items *to* Claude Code, Claude Desktop, or any MCP client — complements, doesn't replace, the above |

## Official surface

### Claude Agent SDK: Python and TypeScript only

The Claude Agent SDK — the "Claude Code as a library" product — ships as:
- `claude-agent-sdk` (Python, PyPI)
- `@anthropic-ai/claude-agent-sdk` (TypeScript/JS, npm)

Both wrap the same underlying agent loop, built-in tools (Read/Write/Edit/Bash/Glob/Grep/WebSearch/WebFetch), MCP support, subagents, permissions, and session management that power the `claude` CLI. **No Rust package exists in this family.** If a Rust project wants "the Agent SDK," the only Anthropic-shipped way to reach it from Rust is the CLI binary.

### The headless `claude` CLI as the real integration boundary

The `claude` CLI's non-interactive mode (`-p`/`--print`) is documented as the CLI surface of the Agent SDK itself — Anthropic's own docs at `code.claude.com/docs/en/headless` describe it as "run the Agent SDK... as a CLI." That makes it a first-party, if indirect, integration point for a Rust caller: spawn `claude`, talk to it over stdio, and you're driving the same agent loop the SDK packages drive.

Verified locally: `claude --version` → `2.1.220 (Claude Code)`.

**Key flags** (from `claude --help`, verified against the installed 2.1.220 binary):

| Flag | Purpose |
|---|---|
| `-p`, `--print` | Non-interactive mode; print result and exit |
| `--output-format <format>` | `text` (default) \| `json` \| `stream-json` (only with `--print`) |
| `--input-format <format>` | `text` (default) \| `stream-json` (only with `--print`) — enables bidirectional streaming input |
| `--include-partial-messages` | Stream token-level deltas (`stream_event` messages) when combined with `--output-format stream-json` |
| `--json-schema <schema>` | Constrain `--output-format json` output to a JSON Schema — the response carries the constrained value in `structured_output` |
| `-r`, `--resume [value]` | Resume a session by ID (or open a picker) |
| `-c`, `--continue` | Continue the most recent conversation in the current directory |
| `--fork-session` | On resume, create a new session ID instead of reusing the original |
| `--session-id <uuid>` | Use a specific session ID |
| `--permission-mode <mode>` | `acceptEdits` \| `auto` \| `bypassPermissions` \| `manual` \| `dontAsk` \| `plan` |
| `--allowedTools` / `--disallowedTools <tools...>` | Permission rules, e.g. `"Bash(git *) Edit"` |
| `--dangerously-skip-permissions` / `--allow-dangerously-skip-permissions` | Bypass all permission checks (sandboxes only) |
| `--mcp-config <configs...>` | Load MCP servers from JSON files or inline JSON strings |
| `--strict-mcp-config` | Only use MCP servers from `--mcp-config`, ignore project/user config |
| `--bare` | Skip hooks, LSP, plugin sync, auto-memory, background prefetches, keychain reads; sets `CLAUDE_CODE_SIMPLE=1`. Anthropic docs recommend this for scripted/SDK calls and note it "will become the default for `-p` in a future release." |
| `--model <model>` | Alias (`opus`, `sonnet`, `fable`) or full model ID |
| `--system-prompt` / `--append-system-prompt[-file]` | Replace or extend the system prompt |
| `--settings <file-or-json>` | Load settings (can supply `apiKeyHelper` here for bare mode) |
| `--forward-subagent-text` | Also emit subagent text/thinking blocks in the stream (only with `--print` + `stream-json`) |
| `--no-session-persistence` | Don't persist the session to disk (only with `--print`) |
| `--max-budget-usd <amount>` | Hard dollar cap on API spend for the run (only with `--print`) |
| `--fallback-model <models>` | Comma-separated fallback chain if the primary model is overloaded |

Hooks (`SessionStart`, `SessionEnd`, etc.) are **not** CLI flags — they're configured in `settings.json`/`.claude/settings.json` and fire automatically during a `-p` run unless `--bare` is passed.

### The stream-json message schema (verified against official docs)

`--output-format stream-json` emits newline-delimited JSON (NDJSON) — one JSON object per line, in order:

1. **`system` / `init`** — first event (unless plugin-install or hook-lifecycle events precede it). Carries `model`, `tools`, `mcp_servers` (each with `name`/`status`), `plugins`, `plugin_errors`, `mcp_server_errors`, and an optional `capabilities` array (feature-detection strings like `interrupt_receipt_v1`).
2. **`system` / `api_retry`** — emitted on a retryable API failure before Claude Code retries. Fields: `attempt`, `max_retries`, `retry_delay_ms`, `error_status`, `error` (category string), `uuid`, `session_id`.
3. **`assistant`** and **`user`** — regular turn messages. Each carries `parent_tool_use_id`: `null` for the main conversation, or the spawning tool-call ID when the message came from a subagent (only emitted by default as `tool_use`/`tool_result` blocks; pass `--forward-subagent-text` to also get subagent text/thinking).
4. **`stream_event`** — only with `--include-partial-messages`; wraps a partial content delta, e.g. `event.delta.type == "text_delta"` with `event.delta.text`.
5. **`result`** — always the **last** line. Carries final response text, cost (`total_cost_usd`, per-model breakdown with `--output-format json`), and session metadata (`session_id`).

**Correction:** the two GitHub issue numbers this doc originally cited here (`anthropics/claude-code#24612` and `#24594`) do not exist — both 404 on the `anthropics/claude-code` repo. The official headless docs (`code.claude.com/docs/en/headless`, fetched 2026-08-03) in fact document the message catalog in detail: `system`/`init` (model, tools, mcp_servers, plugins, plugin_errors, mcp_server_errors, optional `capabilities` array), `system`/`api_retry` (attempt, max_retries, retry_delay_ms, error_status, error, uuid, session_id), `system`/`plugin_install`, `assistant`/`user` messages with `parent_tool_use_id`, `stream_event` (with `--include-partial-messages`), and a trailing `result`. Even so, treat the schema as **stable in shape, evolving in field set across patch versions** (the docs page itself calls out several fields as version-gated, e.g. `capabilities` requiring v2.1.205+, `mcp_server_errors` requiring v2.1.219+) — don't hardcode an allowlist of top-level `type` values; ignore unknown ones.

**Minimal jq recipe** (from the official docs) to stream just the text:

```bash
claude -p "Explain recursion" --output-format stream-json --verbose --include-partial-messages | \
  jq -rj 'select(.type == "stream_event" and .event.delta.type? == "text_delta") | .event.delta.text'
```

**Structured output** for meetrs' use case (extract action items into a fixed shape) doesn't require the stream-json input/output dance at all — plain `--output-format json` plus `--json-schema` does it in one shot:

```bash
claude -p "Extract action items from this transcript as a list of {owner, task, due_date}" \
  --output-format json \
  --json-schema '{"type":"object","properties":{"action_items":{"type":"array","items":{"type":"object","properties":{"owner":{"type":"string"},"task":{"type":"string"},"due_date":{"type":["string","null"]}},"required":["owner","task"]}}},"required":["action_items"]}' \
  < transcript.txt
```
The response's `structured_output` field is the parsed, schema-conforming JSON — parse that field with `serde_json::from_value::<ActionItems>(...)` in Rust and skip hand-parsing the stream for this workload.

### Session resume

`--continue` resumes the most recent conversation in the current directory; `--resume <session_id>` resumes a specific one (session-ID lookup is scoped to the current project directory and its git worktrees). Capture the ID from a prior `--output-format json` run: `session_id=$(claude -p "..." --output-format json | jq -r '.session_id')`. `--fork-session` branches into a new session ID instead of continuing the original one in place.

## Auth: does driving the CLI really avoid an API key?

**Yes, verified against the official docs.** In normal (non-`--bare`) mode, `claude -p` loads the same OAuth/keychain-based Claude Code login a user already has from running `claude` interactively — no `ANTHROPIC_API_KEY` needed. This is a real, meaningful advantage for a desktop tool like meetrs: the user who already pays for Claude Pro/Max and runs Claude Code has zero extra setup.

The catch: `--bare` (Anthropic's own recommendation for "scripted and SDK calls," and slated to become the `-p` default) **explicitly does not use the subscription login** — "Bare mode skips OAuth and the system keychain, so Claude Code only sees credentials you pass explicitly." In bare mode you're back to `ANTHROPIC_API_KEY` (or an `apiKeyHelper` in `--settings`). So the auth story is: **non-bare `-p` reuses subscription auth; `--bare` requires an API key.** For meetrs, staying off `--bare` is exactly what buys the no-API-key advantage, at the cost of `-p` also picking up the user's local hooks/MCP servers/CLAUDE.md unless you don't want that.

Raw HTTP against the Messages API always needs either an `ANTHROPIC_API_KEY` or a bearer token minted from an `ant auth login` profile via `ant auth print-credentials --access-token` (sent as `Authorization: Bearer <token>` plus header `anthropic-beta: oauth-2025-04-20`) — there is no way to use a bare Claude Code subscription login over raw HTTP without going through the `ant` CLI's credential store first. `[unverified]` whether the `ant` CLI ships or is expected on end-user machines the way `claude` is — treat `ant`-based token minting as a developer-side convenience, not something to ask a meetrs end user to set up.

## Community Rust crates (crates.io, verified 2026-08)

Searched crates.io directly (via its JSON API, not the JS-rendered web page) for every candidate named in the brief plus adjacent hits. None of the promising-sounding names in the prompt that don't appear below (`claude-sdk-rs` and `claude-agent-sdk` *do* exist — see the flagged entries) were fabricated; all listed crates below are real, current data as of this research date.

| Crate | Version | Last publish | Total downloads | License | Repo | Notes |
|---|---|---|---|---|---|---|
| `anthropic-sdk` | 0.1.5 | 2024-07-23 | 76,132 | MIT | `mixpeal/anthropic-sdk` | Highest total downloads of the pure-client crates, but stalled at 0.1.5 for 18+ months as of this writing — likely inflated by being an early, generically-named search hit rather than active use. |
| `anthropic-ai-sdk` | 0.2.27 | 2026-01-11 | 43,191 (5,068 recent) | MIT | `katsuhirohonda/anthropic-sdk-rs` | Most recently active of the group (52 published versions), meaningful recent-download volume. Best-maintained of the pure HTTP-client crates. |
| `async-anthropic` | 0.6.0 | 2025-05-03 | 44,774 (7,921 recent) | MIT | `bosun-ai/async-anthropic` | Backed by a company (bosun.ai) rather than a solo maintainer; async-first, Messages API client. |
| `clust` | 0.9.0 | 2024-06-30 | 14,514 | MIT OR Apache-2.0 | `mochi-neko/clust` | Explicitly "unofficial"; has a macro feature (`clust_tool`) for tool definitions; stalled since mid-2024. |
| `misanthropy` | 0.0.8 | 2025-06-08 | 12,624 (317 recent) | MIT | `cortesi/misanthropy` | 0.0.x versioning after two years — maintainer signals it's not stability-committed. |
| `anthropic` | 0.0.8 | 2024-09-03 | 28,740 | — | `abdelhamidbakhta/antrhopic-rs` | Also 0.0.x; oldest crate in the space (created 2023) but never left pre-1.0. |
| `anthropic-sdk-rust` | 0.1.1 | 2025-06-11 | 10,935 | — | `dimichgh/anthropic-sdk-rust` | Single version ever published, no updates since. |
| `anthropic_client` | 1.0.0 | 2024-04-15 | 8,549 (24 recent) | — | `sivanbil/anthropic_client` | "1.0.0" but negligible recent downloads and no repo activity signal. |
| `anthropic-api` | 0.0.5 | 2025-03-25 | 4,900 | — | `Swiftyos/anthropic` | Newer, small, 0.0.x. |

**Honest assessment: every one of these is thin.** None wraps prompt caching headers, extended/adaptive thinking parameters, the Files API, Batches, or an MCP client in a way this research could confirm as first-class — most are a `reqwest` client plus hand-written `serde` structs for the `/v1/messages` request/response shape, sometimes with a manual tool-use loop. Treat all of them as "save yourself the `reqwest` boilerplate," not "get the Anthropic SDK experience." Pick `anthropic-ai-sdk` or `async-anthropic` if you want a dependency at all; otherwise raw HTTP (below) gives you the same capability with no crate lock-in.

**Crates the prompt asked about that don't exist as named:** no `claude-sdk-rs`-adjacent name existed for "the Rust equivalent of the Python/TS Claude Agent SDK" until the two flagged crates below — genai (a multi-provider crate, see below) and rig-core (an agent framework, see below) are the closest real matches to "an agent-building crate that happens to support Claude."

### Flagged: two crates that look official but are not

- **`claude-agent-sdk`** (crates.io, v0.1.1, published 2025-09-30, MIT, 3,776 downloads) lists its repository as `https://github.com/anthropics/claude-agent-sdk-rust`. **That URL 404s — it is not a real repository under the `anthropics` GitHub org.** The crate was published by GitHub user `dhuseby`, not by Anthropic. crates.io does not verify that a listed `repository` field is owned by the publisher, so this metadata is either a mistake or deliberately misleading. Do not treat this crate as Anthropic-authored or Anthropic-endorsed. `[unverified]` whether its actual code is a legitimate independent implementation or largely non-functional — the research budget here didn't extend to auditing the crate's source, and the misleading repo link is reason enough to avoid it regardless.
- **`claude-sdk-rs`** (crates.io, v1.0.2, MIT, repo `bredmond1019/claude-sdk-rs`) is a real, if small, community project (7 commits at last check) that **wraps the `claude` CLI as a subprocess** — i.e., it's a Rust convenience layer over exactly the headless-CLI approach this doc already recommends doing directly. Its README claims support for streaming, tool-use permission gating, and an `mcp` feature flag. Given it's a thin subprocess wrapper with minimal commit history, evaluate it as "maybe saves you 200 lines of subprocess/NDJSON-parsing code," not as a dependency to build critical infrastructure on without reading its source first.

There is also a small constellation of similarly-named, likely low-quality crates (`claude-agents-sdk`, `claude-agent-sdk-rs`, `cc-agent-sdk`, `claude-code-agent-sdk`) discovered via search but not deep-audited here — the naming collision around "claude agent sdk" on crates.io is real and confusing; verify the actual GitHub org before trusting any of them.

### Multi-provider / agent-framework crates (support Claude among others)

| Crate | Version | Downloads (recent) | License | What it is |
|---|---|---|---|---|
| `genai` | 0.7.0-beta.15 | 291,172 (105,817) | MIT OR Apache-2.0 | Multi-provider LLM client (OpenAI, Gemini, Anthropic, Ollama, Bedrock, Vertex, Groq, DeepSeek, GitHub Copilot, more). Still in beta versioning after 2+ years, but very active (updated within the last few days of this research) and by far the most-downloaded Claude-capable Rust crate. Worth evaluating if meetrs might add non-Anthropic model support later. |
| `rig-core` (rig) | 0.41.0 | 1,964,358 (1,270,092) | MIT | "An opinionated library for building LLM powered applications" — an agent-framework crate (RAG, tool use, vector store integrations) with Claude as one of several supported providers. Far higher adoption than any single-provider Anthropic crate; if meetrs wants agent scaffolding rather than a bare client, this is the most credible Rust option in the ecosystem, though it's a bigger architectural commitment than a thin HTTP client. |
| `llm-chain` | 0.13.0 | 93,093 | — | Older "chain LLM calls" library; last published Nov 2023 — effectively unmaintained relative to the other two. Not recommended for new work. |

## Agent Client Protocol (ACP) and Rust

ACP (originated at Zed, Apache-licensed, `agentclientprotocol.com`) standardizes editor↔agent communication — the same problem MCP solves for tool access, but for the editor/IDE side of an agentic coding tool. It is **not** the same thing as MCP and is not what you'd use to expose meetrs' data to Claude Code; it's what you'd implement if meetrs itself wanted to *become* an ACP-compatible agent that editors like Zed can drive, or if meetrs wanted to embed an ACP client to drive Claude Code (or another ACP-speaking agent) the way Zed does.

Rust support is official and well-adopted:
- `agent-client-protocol` — v2.0.0, 3,463,939 total downloads (1,900,468 recent), Apache-2.0, repo `agentclientprotocol/rust-sdk`. This is the reference Rust implementation of both sides of the protocol (agent server and client).
- `claude-code-acp-rs` — v0.1.22, 4,109 downloads, repo `soddygo/claude-code-acp-rs` — a Rust ACP *adapter* specifically for driving Claude Code from an ACP client (e.g. Zed). This is closer to what a Rust tool wanting to *drive* Claude Code via ACP instead of raw subprocess/stream-json would use, though it's a small single-maintainer project with modest adoption.

For meetrs' stated goal (summarize a transcript, extract action items, maybe write to a knowledge base), ACP is the wrong layer — it's for interactive agent-in-editor sessions, not one-shot batch summarization. Worth knowing about, not worth adopting here.

## MCP in Rust: `rmcp`, the official SDK — and the better architecture

**`rmcp`** is the official Rust SDK for the Model Context Protocol, maintained under `modelcontextprotocol/rust-sdk` on GitHub (mirrored/re-published, `github.com/4t145/rmcp` appears to be an earlier or parallel maintainer line — the canonical crate on crates.io points at the `modelcontextprotocol` org repo). Verified crates.io data: **v3.1.0, 18,482,689 total downloads, 9,295,575 recent downloads, Apache-2.0** — by a wide margin the most heavily used Claude/Anthropic-adjacent Rust crate in this entire research pass, reflecting MCP's broad adoption as the standard tool-access protocol, not just Claude-specific usage.

`rmcp` supports building both MCP servers and clients, uses Tokio, supports stdio and SSE/HTTP transports, and provides `#[tool]`/`#[tool_box]`-style macros for defining server tools ergonomically.

**This is very likely the better architecture for meetrs than embedding an LLM client at all.** Instead of meetrs calling out to Claude to summarize a transcript, meetrs can run as a local **MCP server** exposing tools/resources like `list_meetings`, `get_transcript(meeting_id)`, `get_action_items(meeting_id)`, `write_note(meeting_id, content)`. Then:
- The user's existing Claude Code (or Claude Desktop, once it supports local MCP servers, or any other MCP client) calls into meetrs on demand — "summarize my last meeting" becomes a normal Claude Code prompt that fetches the transcript via MCP and reasons over it with the model the user already has configured.
- meetrs never needs its own Anthropic API key, its own model-selection logic, its own prompt-caching tuning, or a dependency on any of the thin community crates above.
- meetrs' own code stays focused on what it's actually good at — audio capture, transcription, structured storage — and delegates "have an LLM reason about this" to whatever the user's LLM tooling already is.

The tradeoff: this makes meetrs *reactive* (something has to ask it) rather than *proactive* (meetrs automatically summarizes right after a meeting ends). If proactive post-meeting summarization is a hard requirement, meetrs still needs to be the caller for that step — which points back to the CLI-subprocess approach in the Recommendation section for that specific flow, while still standing up an MCP server for on-demand querying (e.g., "what did we decide about X in last week's standup").

### Minimal `rmcp` server sketch

```rust
use rmcp::{ServerHandler, model::*, service::RequestContext, tool, tool_router, transport::stdio, ServiceExt};

#[derive(Clone)]
struct MeetrsServer {
    // e.g. a handle to your sqlite/transcript store
}

#[tool_router]
impl MeetrsServer {
    #[tool(description = "Fetch the transcript for a given meeting ID")]
    async fn get_transcript(&self, meeting_id: String) -> Result<CallToolResult, rmcp::Error> {
        let text = self.load_transcript(&meeting_id).await?; // your storage lookup
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    #[tool(description = "List recent meetings with id, title, and date")]
    async fn list_meetings(&self) -> Result<CallToolResult, rmcp::Error> {
        let meetings = self.load_recent_meetings().await?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string(&meetings)?,
        )]))
    }
}

impl ServerHandler for MeetrsServer {
    // implement server_info / capabilities per rmcp's ServerHandler trait
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let server = MeetrsServer { /* ... */ };
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
```

`[unverified]` exact macro/trait signatures against the current `rmcp` 3.1.0 API — the crate has moved fast (58 published versions) and macro ergonomics have shifted across major versions; treat this as illustrative of the shape (tool methods, stdio transport, `ServiceExt::serve`), and check `docs.rs/rmcp` for the exact current API before writing real code against it. Then register the server with Claude Code via `claude mcp add` or a `.mcp.json` entry (`--mcp-config` on the CLI, or the project/user MCP config Claude Code auto-discovers).

## Raw HTTP from Rust: what it actually takes

If you don't want the CLI subprocess or any crate, hitting `POST /v1/messages` directly with `reqwest` + `serde` is the fallback with zero dependency risk. Model IDs, pricing, and parameter shapes below are taken from the `claude-api` skill's cached reference (current as of 2026-06-24), not from memory.

**Model choice for summarization** (per the skill's defaults table): `claude-opus-5` at $5.00 / $25.00 per MTok is the skill's hard default for any Claude task unless the user names another model. For a high-volume, latency-tolerant, cost-sensitive batch job like "summarize every meeting transcript," `claude-sonnet-5` ($3.00 input / $15.00 output per MTok, with an introductory $2.00/$10.00 rate through 2026-08-31) or `claude-haiku-4-5` ($1.00/$5.00 per MTok) are the pragmatic choices for meetrs specifically — summarization and action-item extraction are exactly the kind of workload the skill's own guidance flags as suited to Sonnet-or-below rather than reflexively defaulting to Opus. Only reach for Opus/Fable-tier if summary quality on long, jargon-heavy, multi-speaker transcripts turns out to need it.

### Minimal realistic sketch (non-streaming, tool-free)

```rust
use serde::{Deserialize, Serialize};

#[derive(Serialize)]
struct MessageRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    system: &'a str,
    messages: Vec<Message<'a>>,
}

#[derive(Serialize)]
struct Message<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    kind: String,
    text: Option<String>,
}

#[derive(Deserialize)]
struct MessageResponse {
    content: Vec<ContentBlock>,
    #[allow(dead_code)]
    usage: serde_json::Value,
}

async fn summarize_transcript(
    client: &reqwest::Client,
    api_key: &str,
    transcript: &str,
) -> anyhow::Result<String> {
    let req = MessageRequest {
        model: "claude-sonnet-5",
        max_tokens: 4096,
        system: "You summarize meeting transcripts and extract action items as a bulleted list with owners.",
        messages: vec![Message { role: "user", content: transcript }],
    };

    let resp = client
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .json(&req)
        .send()
        .await?
        .error_for_status()?
        .json::<MessageResponse>()
        .await?;

    let text = resp
        .content
        .into_iter()
        .find(|b| b.kind == "text")
        .and_then(|b| b.text)
        .unwrap_or_default();
    Ok(text)
}
```

### Streaming (SSE) by hand

There is no official Anthropic SSE client for Rust. `reqwest`'s response body is a `Stream<Item = Result<Bytes, reqwest::Error>>`; you'd feed that through a line-splitter (SSE frames are `event: ...\ndata: {...}\n\n`) and `serde_json::from_str` each `data:` payload into an enum matching the Messages API's stream event types (`message_start`, `content_block_start`, `content_block_delta`, `content_block_stop`, `message_delta`, `message_stop`) — the same event vocabulary documented in the `claude-api` skill's streaming reference for other languages, just with no Rust-side deserialization types provided for you. `eventsource-stream` or `async-sse` (both small, general-purpose SSE-parsing crates, unrelated to Anthropic) can carry the line-framing work; you still own the JSON schema.

### Tool-use loop and prompt caching, by hand

- **Tool-use loop:** send `tools: [...]` in the request body per the standard JSON Schema shape; on a `stop_reason: "tool_use"` response, extract the `tool_use` content block(s), execute them locally, and send a follow-up request with the full prior assistant turn plus a `tool_result` user turn. This is exactly the "manual agentic loop" pattern documented for every official SDK language — Rust just has no `tool_runner()` helper to do it for you, so you write the loop yourself (maybe 40-60 lines).
- **Prompt caching:** add `"cache_control": {"type": "ephemeral"}` to the `system` block (as an array-of-blocks rather than a bare string) or to specific message content blocks. No SDK magic needed — it's just extra JSON fields, verify hits via `usage.cache_read_input_tokens` in the response. If meetrs sends the same long system prompt or a repeated multi-transcript context on every summarization call, this is a real cost lever worth the ~10 lines it takes to wire up in raw JSON.

## Open questions

- **Exact `rmcp` 3.1.0 macro/trait API** — the sketch above is illustrative; confirm current `#[tool]`/`ServerHandler`/transport signatures against `docs.rs/rmcp` before writing production code, since the crate has iterated through many major versions.
- **Whether Claude Desktop (not just Claude Code) will support local/stdio MCP servers the way this doc assumes** for the "on-demand query" architecture — if meetrs' target user drives everything through Claude Code specifically, this is moot; if the target is broader ("any Claude surface"), verify current Desktop MCP support.
- **Whether the `ant` CLI (used to mint a bearer token from a Claude Code login for raw-HTTP calls) is something meetrs could reasonably ask an end user to install**, or whether that path is realistically developer-only — this affects how strongly to lean on "reuse subscription auth" for the raw-HTTP fallback path specifically (the CLI-subprocess path doesn't need `ant` at all).
- **Actual code quality/functionality of `claude-sdk-rs` (bredmond1019) and the broader unofficial-crate field** — this research verified metadata (versions, downloads, licenses, repo existence) via crates.io's API and spot-checked one repo's README via WebFetch, but did not audit source code line-by-line for any community crate. Before depending on one, read it.
- **Rate of change in the stream-json schema** — the two GitHub issues this doc originally cited for "undocumented message types" don't exist (verified 404, see fact-check log below); the official docs page is comprehensively documented as of 2026-08-03. Individual fields are still version-gated across Claude Code releases (see the docs page's per-field minimum-version notes), so don't assume every field is present on every installed version.

## Sources

- [Run Claude Code programmatically — official headless-mode docs](https://code.claude.com/docs/en/headless)
- [`claude --help` and `claude --version`](file:///dev/stdin) — verified locally, installed version 2.1.220
- [crates.io API](https://crates.io/api/v1/crates/) — queried directly for every crate's version/downloads/license/repository metadata (misanthropy, clust, anthropic-ai-sdk, async-anthropic, anthropic-sdk-rust, anthropic-sdk, anthropic_client, anthropic-api, anthropic, rmcp, genai, rig-core, llm-chain, agent-client-protocol, claude-code-acp-rs, claude-sdk-rs, claude-agent-sdk)
- [GitHub — anthropics/claude-agent-sdk-rust (404, does not exist)](https://github.com/anthropics/claude-agent-sdk-rust)
- [GitHub — bredmond1019/claude-sdk-rs](https://github.com/bredmond1019/claude-sdk-rs)
- [GitHub — modelcontextprotocol/rust-sdk (rmcp)](https://github.com/modelcontextprotocol/rust-sdk)
- [GitHub — agentclientprotocol/rust-sdk (ACP)](https://github.com/agentclientprotocol/rust-sdk)
- `claude-api` skill (bundled reference, cached 2026-06-24) — model IDs, pricing table, prompt-caching and tool-use mechanics used for the raw-HTTP section
- [crates.io API — per-crate metadata](https://crates.io/api/v1/crates/) — re-verified 2026-08-03 for `claude-agent-sdk`, `claude-sdk-rs`, `rmcp`, `agent-client-protocol`, and every community Anthropic-client crate listed above; every download/version/repo figure matched the original research exactly
- [GitHub API — `bredmond1019/claude-sdk-rs` commit history](https://api.github.com/repos/bredmond1019/claude-sdk-rs/commits) — confirmed 7 commits total (2026-08-03)
- [crates.io — `claude-agent-sdk` owners endpoint](https://crates.io/api/v1/crates/claude-agent-sdk/owners) — confirmed publisher is GitHub user `dhuseby`, not an Anthropic account (2026-08-03)

## Fact-check log (2026-08-03)

**Method:** ran `claude --version` / `claude --help` locally; fetched `code.claude.com/docs/en/headless` live; loaded the bundled `claude-api` skill as the authoritative pricing/model reference; re-queried the crates.io API directly (with a proper User-Agent) for every crate cited; checked GitHub for repo existence, commit counts, and crate-owner attribution; checked the two cited GitHub issue numbers.

### CONFIRMED
- **Local version**: `claude --version` → `2.1.220 (Claude Code)`. Matches the doc's claim exactly.
- **`--json-schema` flag exists** and behaves as described: constrains `--output-format json` output to a JSON Schema, with the parsed value returned in the response's `structured_output` field. Confirmed against both local `claude --help` and the live official docs. The doc's claim that this is a real, working flag is correct — it was not fabricated.
- **All other CLI flags the doc names** (`-p`/`--print`, `--output-format`, `--input-format`, `--include-partial-messages`, `-r`/`--resume`, `-c`/`--continue`, `--fork-session`, `--session-id`, `--permission-mode` and its exact choice list, `--allowedTools`/`--disallowedTools`, `--dangerously-skip-permissions`, `--mcp-config`, `--strict-mcp-config`, `--bare`, `--model`, `--system-prompt`/`--append-system-prompt[-file]`, `--forward-subagent-text`, `--no-session-persistence`, `--max-budget-usd`, `--fallback-model`) — all verified present in the local `claude --help` output with matching descriptions.
- **Hooks are not CLI flags** — confirmed; they're configured in settings files and fire automatically during `-p` runs unless `--bare` is passed.
- **Stream-json message schema** — `system`/`init` fields (model, tools, mcp_servers, plugins, plugin_errors, mcp_server_errors, optional `capabilities`), `system`/`api_retry` fields (attempt, max_retries, retry_delay_ms, error_status, error, uuid, session_id), `assistant`/`user` with `parent_tool_use_id`, `stream_event` (partial-message deltas), and a trailing `result` — all confirmed verbatim against the live official docs page.
- **Auth claim** — confirmed against official docs: non-`--bare` `-p` reuses the existing Claude Code OAuth/keychain login (no API key needed); `--bare` strictly requires `ANTHROPIC_API_KEY` or an `apiKeyHelper`, never reading OAuth/keychain. The doc's framing of this tradeoff is accurate.
- **Model IDs and pricing** — `claude-sonnet-5` ($3.00/$15.00, $2.00/$10.00 intro through 2026-08-31), `claude-haiku-4-5` ($1.00/$5.00), `claude-opus-5` ($5.00/$25.00) — all match the `claude-api` skill's authoritative pricing table exactly, including the exact ID strings.
- **"No official Rust SDK" / Agent SDK language claim** — confirmed via web search and the official docs: the Claude Agent SDK ships only as Python (`claude-agent-sdk`) and TypeScript (`@anthropic-ai/claude-agent-sdk`) packages. A Go SDK is an open, unresolved feature request (`anthropics/claude-agent-sdk-python#498`), not a shipped package. No Rust package exists.
- **`claude-agent-sdk` crate accusation** — fully confirmed, all three parts: (1) `https://github.com/anthropics/claude-agent-sdk-rust` returns HTTP 404 — verified directly. (2) The crate's registered owner on crates.io is GitHub user `dhuseby` (Dave Grantham), not any Anthropic-affiliated account. (3) Download/version metadata (v0.1.1, 3,776 total downloads) matches exactly. This is a real, verified finding — kept as-is.
- **`claude-sdk-rs` (bredmond1019) claim** — confirmed as a CLI-subprocess wrapper with minimal history: GitHub API confirms exactly 7 commits total on the repo.
- **Every crates.io metadata figure in the doc** (`anthropic-sdk`, `anthropic-ai-sdk`, `async-anthropic`, `clust`, `misanthropy`, `anthropic`, `anthropic-sdk-rust`, `anthropic_client`, `anthropic-api`, `genai`, `rig-core`, `claude-code-acp-rs`) — every single total-download, recent-download, version, and repository figure matched the live crates.io API exactly on re-query. No discrepancies found.
- **`rmcp` figures** — v3.1.0, 18,482,689 total downloads, 9,295,575 recent, repo `modelcontextprotocol/rust-sdk` — confirmed exactly.
- **`agent-client-protocol` figures** — v2.0.0, 3,463,939 total, 1,900,468 recent — confirmed exactly. The task flagged these numbers as "seems high" for extra scrutiny; they check out as accurate, not inflated.
- **Code sketches** (`rmcp` server sketch, raw `reqwest`/`serde` Messages API sketch, SSE event vocabulary) — shapes match the real Messages API request/response format and the real `rmcp` tool/transport pattern; the doc itself already flags the `rmcp` macro/trait signatures as `[unverified]` pending a `docs.rs` check, which remains appropriately hedged rather than asserted as fact.

### CORRECTED
- **Two fabricated GitHub issue citations** (said → true): the doc claimed Anthropic had an open tracking issue `anthropics/claude-code#24612` asking for full stream-json message-type documentation, plus a second issue `#24594` about `--input-format stream-json` being undocumented, and cited both as sources → **both return HTTP 404 on the `anthropics/claude-code` repository; neither issue exists.** Source: direct `gh api` lookup, 2026-08-03. Removed both citations from the Sources list and rewrote the two passages that relied on them (stream-json section and Open Questions) to instead state plainly that the official docs page is comprehensively documented as of this fact-check, while still noting that individual fields are version-gated across Claude Code releases (which the docs page itself specifies with per-field minimum-version annotations).

### STILL UNVERIFIED
- The exact current `rmcp` 3.1.0 macro/trait API (`#[tool]`, `ServerHandler`, transport signatures) — the doc already marks this `[unverified]` and recommends checking `docs.rs/rmcp` before writing production code; this fact-check did not audit the crate's source or docs.rs page.
- Whether the `ant` CLI is realistically something to ask a meetrs end user to install, versus a developer-only convenience — already marked `[unverified]` in the doc; out of scope for this pass.
- Whether Claude Desktop (as opposed to Claude Code) supports local/stdio MCP servers — already flagged as an open question in the doc; not re-verified here.
- Actual code quality of the community crates beyond metadata (versions/downloads/licenses/repo existence) — this fact-check, like the original research, did not audit crate source code line-by-line.

## Recommendation: unchanged

Nothing in this fact-check contradicts the original Recommendation. The only substantive correction (two fabricated GitHub issue citations) affected supporting evidence for a side point about schema documentation completeness, not any load-bearing claim about CLI flags, pricing, model IDs, or the `claude-agent-sdk` crate accusation — all of which were independently confirmed. The CLI-subprocess-plus-MCP-server architecture recommendation stands as originally written.
