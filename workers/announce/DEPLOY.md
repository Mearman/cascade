# Deploying the cascade-announce Cloudflare Worker

The announce Worker hosts cascade's peer-discovery directory on Cloudflare's edge at zero operational cost. It implements exactly the same HTTP contract as the `--announce-bind` endpoint on `cascade-relay` — a device POSTs its signed candidate set and any peer GETs it by device ID — with Workers KV as the soft-state store rather than an in-memory hash map.

Full deployment context is in [`docs/deployment.md`](../../docs/deployment.md) at the repository root.

## Prerequisites

- A Cloudflare account with a Workers plan (the free tier is sufficient for a personal mesh).
- `wrangler` CLI: `npm install -g wrangler` or `pnpm add -g wrangler`.
- `worker-build`: installed automatically by the `wrangler deploy` build command.

Authenticate wrangler once:

```sh
wrangler login
```

## Step 1 — create the KV namespace

The Worker stores candidate sets in a Workers KV namespace called `ANNOUNCE`. Create it once:

```sh
wrangler kv:namespace create ANNOUNCE
```

Wrangler prints something like:

```
{ binding = "ANNOUNCE", id = "abc123def456..." }
```

Copy the returned `id` and paste it into `wrangler.toml`:

```toml
[[kv_namespaces]]
binding = "ANNOUNCE"
id = "abc123def456..."   # ← replace REPLACE_WITH_KV_NAMESPACE_ID with this
```

The `wrangler.toml` already has the right binding name and TTL variable; only the KV namespace ID needs filling in.

## Step 2 — set the HMAC shared secret

The announce directory gates writes with an HMAC-SHA256 tag. The secret must match what each announcing device is configured with. Set it as a Worker secret so it is never stored in `wrangler.toml` or source control:

```sh
wrangler secret put ANNOUNCE_SHARED_SECRET
```

Wrangler prompts for the value. Paste the same 64-character hex secret the relay uses (or generate a fresh one with `openssl rand -hex 32` if the announce Worker is the only carrier). The secret is stored encrypted in Cloudflare's secrets store and injected into the Worker at runtime; it never appears in logs or response bodies.

## Step 3 — deploy

```sh
wrangler deploy
```

This runs `cargo install worker-build && worker-build --release` (as specified in `wrangler.toml`), compiles the crate to `wasm32-unknown-unknown`, and deploys to Cloudflare. The first deploy takes a few minutes while worker-build downloads and compiles; subsequent deploys are faster.

The deployed Worker URL follows the pattern `https://cascade-announce.<your-account>.workers.dev`.

## Step 4 — verify

```sh
# Should return {"signed":null} for an unknown device ID — not a 404.
curl https://cascade-announce.<your-account>.workers.dev/announce/TESTDEVICE
```

A `{"signed":null}` response confirms the Worker is live and the KV binding is connected. A `503` means the secret binding or the KV namespace ID is wrong.

## Pointing cascade at the Worker

In each device's P2P backend configuration, add the deployed Worker URL as an announce server:

```toml
# ~/.config/cascade/backends/p2p.toml  (or equivalent)

[p2p]
# The exposure posture must be Public for announce-server discovery to run.
exposure = "public"

[[p2p.announce_servers]]
# Base URL of the announce Worker — no trailing slash.
# Do NOT bake in a specific public hostname as a default.  Operator-supplied only.
base_url = "https://cascade-announce.<your-account>.workers.dev"
# The same hex secret configured via `wrangler secret put ANNOUNCE_SHARED_SECRET`.
# Generate with: openssl rand -hex 32
shared_secret = "your-64-char-hex-secret"
```

See [`docs/deployment.md`](../../docs/deployment.md) for how announce servers, relays, and the exposure posture work together.

## `wrangler.toml` reference

```toml
name = "cascade-announce"
main = "build/worker/shim.mjs"
compatibility_date = "2024-09-23"

[build]
command = "cargo install -q worker-build && worker-build --release"

[[kv_namespaces]]
binding = "ANNOUNCE"
id = "REPLACE_WITH_KV_NAMESPACE_ID"   # ← from step 1

[vars]
ANNOUNCE_TTL_SECONDS = "3600"          # per-write KV expiry; 60-second floor enforced by Cloudflare
```

`ANNOUNCE_SHARED_SECRET` is deliberately absent from `[vars]` — it is a secret, not a variable, and must be set via `wrangler secret put`.

## Soft-state semantics

The KV store is eventual-consistent with per-key expiry. Every write sets `expiration_ttl` so entries age out automatically without any background job. A restart of the Worker (Cloudflare does this transparently) loses nothing because the Worker holds no in-memory state — everything is in KV. An announcing device that republishes on a loop will keep its entry fresh; a device that goes offline will see its entry expire within `ANNOUNCE_TTL_SECONDS`.
