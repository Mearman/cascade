# Cascade daemon HTTP JSON API contract — v1

The PWA drives the daemon over a typed JSON HTTP API. The API is a
separate workspace crate (`crates/cascade-web-api`) the daemon wires up
when the operator passes `--web`. The PWA itself is a self-contained
static bundle the operator hosts and points the daemon at via
`--web-bundle-url`; the daemon serves the API and a small manifest
describing the bundle, but does not embed or transpile the PWA.

This document is the v1 contract. It is the source of truth for the
implementer. The F1–F4 directional data-sharing fix is in scope: data
verbs (`data:read`, `data:write`) are first-class in the API surface
and the data-plane gate is unchanged.

British English. No emojis. No magic numbers. The implementation lives
up to this contract; this contract is the load-bearing specification,
not a sketch.

## Scope and non-goals

- A typed JSON front door for the v1 PWA. Not a general
  remote-administration API: `cascade remote <device-id>` over BEP
  remains the management plane; the HTTP API reuses its auth and
  audit machinery rather than introducing a parallel one.
- Request / response only. SSE / WebSocket are v2.
- No back-compat. v0 does not exist; v1 is the only shape.

## Crate layout

```
crates/cascade-web-api/
  Cargo.toml
  src/
    lib.rs              // crate root, re-exports
    state.rs            // AppState, BindConfig, NodeIdentity
    auth.rs             // bearer extraction, session derivation
    error.rs            // ApiError, error envelope, status mapping
    request_id.rs       // per-request id middleware
    router.rs           // build_router, serve, RouterHandle
    routes/             // one module per resource
    schemas/            // request + response types (one module per resource)
```

Dependencies: `axum`, `tower-http`, `serde`, `serde_json`, `tokio`,
`chrono`, `cascade-engine`, `cascade-p2p`, `tracing`, `tracing-subscriber`,
`thiserror`. The daemon crate adds a feature-gated dependency so `axum`
does not pull in when the operator never asks for `--web`.

## Auth — bearer token only, one shape, three classes

One header: `Authorization: Bearer <token-json>`. The token is the same
signed `CapabilityToken` the BEP management plane already verifies. No
new credential format. No API keys, no cookies, no daemon-issued
session ids. The bearer presents a portable, offline-issuable grant;
the daemon verifies it the same way the BEP dispatcher does and routes
the request through the same
`authorises(grants, caller, needed, target, now)` decision.

Data-verb tokens are valid for both the BEP data plane and the HTTP
API. The HTTP layer does not introduce a second authorisation path.

### Three session classes from one token

The verified claims derive one of three classes. The class is
denormalised into `SessionResponse.abilities` for the PWA's first-load
render; the server re-checks every request, and the PWA never trusts
the cached abilities for a decision the server did not re-confirm.

| Class | Condition | Abilities |
|-------|-----------|-----------|
| `owner` | Verified token's `bearer` equals this node's own device id, OR the token was issued by this node and presented back | Every capability on every scope the node has authority over |
| `named_user` | Verified token's `bearer` is a configured device id (any device id named in `config.toml`, a grant row, or a peer) | Whatever capabilities and scopes the verified claims carry |
| `bearer` | Verified token's `bearer` is not in the configured device id set | Whatever the verified claims carry, and only those |

The `named_user` / `bearer` split is a UI signal, not a security
boundary. Both are gated solely by the verified claims. The class only
tells the PWA what to render. `owner` is necessary (not sufficient)
for the dangerous capabilities: the server enforces the underlying
`authorises` check, not a class-based bypass.

### Bearer-binding check

`CapabilityToken::verify` requires the token's `bearer` to equal the
device id the transport authenticated. The HTTP transport does not
authenticate a device id, so the HTTP equivalent is the
`X-Cascade-Bearer-Device` header: the daemon compares the header to
the verified claim. Mismatch is `401` with `code: "bearer_mismatch"`.
The header is mandatory for every authenticated request. The only
unauthenticated routes are `/v1/health` and `/v1/bundle`.

`MAX_TOKEN_JSON_BYTES` (64 KiB) and `MAX_DELEGATION_DEPTH` (8) from
`cascade-engine::manage::token` apply unchanged. A larger or deeper
chain is `413 token_too_large` or `400 chain_too_deep`.

### CORS

`tower-http::cors::CorsLayer` with an allowlist built from two
sources:

1. **Loopback always.** `http://localhost:*`, `http://127.0.0.1:*`,
   `http://[::1]:*` on every port. Always permitted; not configurable.
2. **Operator-configured allowlist.** A `Vec<String>` from
   `[web].cors-origins` in `config.toml` and `--web-cors-origin <url>`
   flags (repeatable). Wildcard `*` is rejected at config-parse time:
   a wildcard CORS allowlist combined with bearer auth is a
   credential-leak footgun.

Methods: `GET`, `HEAD`, `POST`, `PUT`, `DELETE`, `OPTIONS`. Headers:
`Authorization`, `Content-Type`, `X-Cascade-Bearer-Device`,
`X-Cascade-Request-Id`. `Access-Control-Allow-Credentials: false`.
Max age: 600 seconds.

## Error envelope

Every error response, regardless of handler, is one shape:

```json
{
  "error": {
    "code": "unauthorised",
    "message": "presented capability token rejected: expired at 2026-06-04T12:00:00Z (now 2026-06-04T13:00:00Z)",
    "request_id": "01HXY...BASE32",
    "details": {
      "token_id": "ABCDEF...",
      "reason": "expired"
    }
  }
}
```

- `code` (string, required): a stable, machine-readable identifier
  from the closed set below. The PWA branches on this, never on
  `message`.
- `message` (string, required): human-readable, safe to log, not part
  of the contract.
- `request_id` (string, required): the per-request id, 26-character
  base32 (no padding), Crockford alphabet.
- `details` (object, optional): structured context per code. Omitted
  when empty.

HTTP status mapping:

| `code` | Status | When |
|--------|--------|------|
| `unauthorised` | 401 | Token missing, malformed, signature bad, expired, revoked, bearer mismatch, or claims do not satisfy the route's required capability |
| `forbidden` | 403 | Caller has the capability but not over the requested scope |
| `not_found` | 404 | Path or resource does not exist |
| `conflict` | 409 | Optimistic concurrency, duplicate key, already-revoked token |
| `gone` | 410 | Resource existed and was removed |
| `payload_too_large` | 413 | Body or token exceeds the size ceiling |
| `unprocessable` | 422 | Body parsed but failed domain validation (unknown capability, bad scope, etc.) |
| `rate_limited` | 429 | Token-bucket exhausted; `Retry-After` header carries the seconds hint |
| `internal` | 500 | Unexpected server error; details suppressed in production |
| `unavailable` | 503 | Daemon shutting down, state DB unreadable, data-plane readiness bit (F3) not yet set |
| `timeout` | 504 | Upstream backend or engine call exceeded its budget |

Enforced by a single `impl IntoResponse for ApiError` in `error.rs`;
handlers never construct `StatusCode` directly.

## Conventions

- **Request id.** Every request reaches a handler with a request id,
  either echoed from the caller's `X-Cascade-Request-Id` header (when
  it parses as a valid 26-char base32 token) or minted by the daemon.
  The middleware stamps it on the response and on every `tracing`
  span. The PWA sends one on every fetch so support tickets quote an
  id the daemon log can grep.
- **Pagination.** `limit` (1..=200, server clamps) and `cursor`
  (opaque base64url from the previous page's `next_cursor`).
- **Optimistic concurrency.** `If-Match: <etag>` and `If-None-Match`
  on mutating routes that touch versioned resources. Etags are
  base32(SHA-256 of the canonical JSON form). Mismatch is `412
  precondition_failed`.
- **JSON.** Object keys are snake_case (Rust uses
  `#[serde(rename_all = "snake_case")]`). Timestamps are RFC 3339
  strings in UTC. Enums are tagged unions over a `kind` string
  discriminator. `null` is the canonical absent-value marker.

## Routes

Every route is mounted under `/v1`. v1 is the entire contract; no v0,
no unversioned aliases. Each entry lists method/path, required
capability, brief body and response, error codes.

### Health, readiness, bundle

- `GET /v1/health` — capability: none. Returns
  `{ "status": "ok", "version": "...", "node_device_id": "..." }`.
  Always 200 once the daemon is serving; a degraded state shows up
  in `/v1/ready`.
- `GET /v1/ready` — capability: any verified session. Returns
  `{ "ready": true, "data_plane_ready": true, "backends": [...],
  "started_at": "..." }`. Returns `503 unavailable` (with the
  specific reason in `details`) when starting up, when the F3 bit is
  not yet set, or when the state DB is unreadable.
- `GET /v1/bundle` — capability: none. Public manifest for the PWA
  shell. Returns
  `{ "bundle_url": "https://pwa.example.com", "api_base_url":
  "http://localhost:7842", "version": "...", "build_sha": "..." }`.
  The daemon does not serve the bundle. If `--web-bundle-url` is not
  set, `bundle_url` is `null` and the PWA renders a config-error
  screen.

### Session

- `GET /v1/session` — capability: any verified session. Returns the
  verified session:

  ```json
  {
    "session": {
      "class": "owner",
      "node_device_id": "ABCD...EFGH",
      "verified_bearer": "WXYZ...1234"
    },
    "token": {
      "token_id": "ABCDEF...",
      "issuer": "ABCD...EFGH",
      "bearer": "WXYZ...1234",
      "capability": "status:read",
      "scope": { "kind": "node" },
      "expires": "2026-07-04T00:00:00Z",
      "issued_at": "2026-06-04T00:00:00Z"
    },
    "abilities": {
      "status_read": true,
      "pin_write": true,
      "cache_manage": true,
      "config_push": true,
      "policy_set": true,
      "backend_manage": true,
      "lifecycle_control": true,
      "grant_admin": true,
      "data_read": ["/work", "/personal"],
      "data_write": ["/work"]
    }
  }
  ```

  `abilities.data_read` and `data_write` are **arrays of folder
  prefixes**, not booleans — the PWA renders a per-folder sharing
  badge. Folders are the canonical BEP folder ids (`p2p-<name>`), the
  namespace the F1 fix binds grant scope to. The `*_manage` booleans
  are all-or-nothing; the dangerous verbs apply over a folder rather
  than a list.

- `POST /v1/session/revoke` — capability: any verified session. Logs
  the verified token out. The daemon is stateless about sessions, so
  this returns the same `SessionResponse` body with a synthetic
  expired class; the PWA clears its local copy and redirects to
  login. Not a security boundary.

### Files and folders

All routes under `/v1/files/{folder}/...` require `data:read` (read)
or `data:write` (write) over the folder. All routes under
`/v1/folders/{folder}/...` require `status:read` over the folder.
`{folder}` is the canonical BEP folder id (`p2p-<name>`); unknown
folders are `404 not_found`.

- `GET /v1/folders/{folder}/children?path=&limit=&cursor=` — list
  directory entries. Response:
  `{ "folder": "p2p-shared", "path": "reports", "entries": [...],
  "next_cursor": null }`. `size` is `null` for directories.
- `GET /v1/folders/{folder}/entries/{path}` — entry metadata
  (kind, size, mtime, etag).
- `GET /v1/files/{folder}/entries/{path}` and `HEAD` — file content
  with `Content-Type` and `ETag`.
- `PUT /v1/files/{folder}/entries/{path}` — file content.
  `If-Match: <etag>` optional for optimistic concurrency.
  `X-Cascade-Mtime: <rfc3339>` optional for client mtime (clamped
  against the server clock ± 24 h). Response carries new etag and
  mtime. Errors: `unauthorised`, `forbidden`, `not_found`, `conflict`,
  `payload_too_large` (body exceeds `[web].max_body_bytes`, default 1
  GiB), `unavailable`.
- `DELETE /v1/files/{folder}/entries/{path}` — `204` on success.
- `GET /v1/folders/{folder}/archive` — streams `.tar.gz` of the
  subtree as the response body. `Content-Type: application/gzip`;
  `Content-Disposition: attachment; filename="<folder>.tar.gz"`. The
  only v1 streaming route; server-side timeout from
  `[web].request_timeout_secs` (default 3600).
- `GET /v1/folders/{folder}/search?q=&limit=` — substring match on
  entry name. v1 is substring only; glob / regex are v2.

### Shares (data-verb grants)

- `GET /v1/shares` — capability: any verified session. Returns the
  operator-facing view of every data-verb grant, denormalised into a
  per-peer / per-folder posture:

  ```json
  {
    "shares": [
      {
        "peer_device_id": "WXYZ...1234",
        "folder": "shared",
        "folder_id": "p2p-shared",
        "posture": "read-only",
        "granted_by": "ABCD...EFGH",
        "expires": null,
        "grant_ids": [12, 13]
      }
    ]
  }
  ```

  `posture` is one of `read-only`, `write-only`, `read-write`,
  derived from the data-verb grants the peer holds for the folder.

- `POST /v1/shares` — capability: `grant:admin` over the folder.
  Body: `{ "peer_device_id": "...", "folder": "shared", "posture":
  "read-only", "expires": "..." }`. The daemon resolves `folder` to
  `p2p-<name>` (F1 fix) and refuses unknown / non-P2P names with
  `404 not_found` and `details.folders_known`. The underlying grants
  are the data verbs the posture maps to; the audit row is written
  with `actor_device` set to the verified bearer and the request id
  stamped on the `command` column. Response 201 carries the
  canonicalised share.

- `DELETE /v1/shares/{id}` — capability: `grant:admin` over the
  folder. Revokes both verbs in a single transaction so the posture
  drops to `none` atomically. `{id}` is one of the underlying grant
  row ids. `204` on success.

### Tokens (portable capability credentials)

- `GET /v1/tokens` — capability: any verified session. Lists every
  token this node has issued, with `revoked: bool`.
- `POST /v1/tokens` — capability: the capability being conferred,
  plus grant-admin-equivalent authority for the issuing device.
  Concretely: `Owner` can issue any token; `NamedUser` / `Bearer` can
  only delegate authority they themselves hold (the same
  `claims.contains` rule `CapabilityToken::delegate` enforces).
  Body:
  `{ "bearer": "...", "capability": "data:read", "scope": { "kind":
  "folder", "path": "p2p-shared" }, "expires": "..." }`. The F4 bar
  blocks `data:read` / `data:write` over `{ "kind": "node" }` with
  `422 unprocessable` and `code: "data_verb_node_wide_forbidden"`.
  Call-overreach is `422` with `code: "delegation_exceeds_parent"`.
  Response 201 carries the issued token in its JSON form so the
  caller can carry it.
- `POST /v1/tokens/{id}/revoke` — capability: `Owner`. Response 200
  with `revoked_at`; `410 gone` if already revoked.

### Grants, audit, peers, pins, policies, backends, cache, config

- `GET /v1/grants` — capability: any verified session. Lists every
  capability grant row.
- `POST /v1/grants` — capability: `grant:admin` over the scope. The
  F4 bar applies (data verbs over node-wide are
  `data_verb_node_wide_forbidden`); the dangerous-capability bar
  applies; the canonical-folder rule applies (unknown or non-P2P
  folders are `unprocessable` with `unknown_folder`).
- `DELETE /v1/grants/{id}` — capability: `grant:admin` over the
  grant's scope. `204` on success.
- `GET /v1/audit?since=&limit=&cursor=` — capability: `status:read`.
  Reads the same `manage_audit` table the BEP dispatcher writes to.
  Each entry: `{ id, timestamp, actor_device, capability, scope,
  command, outcome, request_id }`. `request_id` is added by this
  contract; existing rows have `request_id: null` until a one-shot
  back-fill migration on first startup.
- `GET /v1/peers` — capability: `status:read`. Returns peer list
  with `data_verb_grants` (the F1 grant columns per folder) and
  `explicit_control` (the F2 `data_explicit_control` table — every
  folder where the peer has ever presented a verified data-verb
  token, even after the token is revoked or expires).
- `GET /v1/pins` / `POST /v1/pins` / `DELETE /v1/pins/{id}` —
  capability: `pin:write` for write, `status:read` for read. Same
  engine `pin_rules` table the CLI writes to.
- `GET /v1/policies` / `POST /v1/policies` / `DELETE
  /v1/policies/{id}` — capability: `policy:set` for write,
  `status:read` for read. Same engine path.
- `GET /v1/backends` — capability: `status:read`. Returns the
  `backends` table. `folder_id` is the canonical BEP folder id; null
  for non-P2P backends. The PWA uses this as the folder picker.
- `POST /v1/cache/evict` and `POST /v1/cache/warm` — capability:
  `cache:manage`. `warm` body: `{ "path_glob": "..." }`. `evict` has
  no body. Both route through the engine's `manage_cache_evict` /
  `manage_cache_warm` and return the engine's short summary string.
- `POST /v1/config/push` — capability: `config:push` over the target
  folder. Body: `{ "folder": "/work", "format": "toml", "body":
  "..." }`. `format` is one of `gitignore`, `toml`, `yaml`, `json`.

## Typed client generation

The PWA needs a typed client. Two viable options:

1. **Hand-maintained TypeScript types** in
   `apps/web/src/api/types.ts`, kept in lockstep with the Rust
   schemas by review. Cheaper to start, no codegen pipeline, the
   types live next to the PWA code, and
   `serde(rename_all = "snake_case")` makes field names match JSON
   1:1.
2. **JSON Schema export + quicktype generation** — `cascade-web-api`
   derives `schemars::JsonSchema` on every schema type and emits
   `docs/api-schema/<resource>.json` at build time; the PWA repo
   runs quicktype against the snapshot; CI fails on drift.

The v1 choice is **option 1**, with `schemars` schemas emitted as
reference and a future codegen entry point, but no automated
pipeline. Reasoning: the PWA is small (< 30 lines per resource),
hand-maintained types let the PWA put `readonly` and branded types
on the TS side without negotiation with the generator, and the
contract test (below) catches Rust-side drift. v2 is free to switch
to quicktype once the surface stops moving.

## Configuration and bind

`cascade start` gains three flags, all no-ops when omitted:

```
cascade start --web
cascade start --web --web-bind 127.0.0.1:7842
cascade start --web --web-bundle-url https://pwa.example.com
cascade start --web --web-cors-origin https://app.example.com   # repeatable
```

`config.toml` mirrors the flags under a `[web]` table:

```toml
[web]
enabled = true
bind = "127.0.0.1:7842"
bundle_url = "https://pwa.example.com"
cors_origins = ["https://app.example.com"]
request_timeout_secs = 3600
max_body_bytes = 1073741824
```

Defaults: `bind = "127.0.0.1:7842"` (loopback only); `cors_origins = []`
(loopback always allowed); `request_timeout_secs = 3600`;
`max_body_bytes = 1073741824` (1 GiB).

`bind = "0.0.0.0"` requires an explicit opt-in and prints a loud
startup warning — a bearer-auth API on a public interface without
TLS is a credential-leak footgun. The daemon refuses to start with
`bind = "0.0.0.0"` and `bundle_url = null`.

## v1 cut

Ships in v1:

- All routes under `/v1/`.
- The auth surface (one header, three classes).
- CORS with loopback-always + operator allowlist.
- The error envelope and status mapping.
- The `[web]` config table and the three CLI flags.
- The `cascade-web-api` workspace crate.

Deferred to v2:

- SSE / WebSocket streams (live change feeds, sync progress).
- `Range` support on `GET /v1/files/...`.
- `PATCH` verbs.
- A `cascade web` CLI subcommand.
- The quicktype codegen pipeline.
- A typed JS client library published to npm.

## v1 MUST NOT

1. **No new credential format.** The HTTP API authenticates with
   `CapabilityToken` only. No API keys, no cookies, no daemon-issued
   sessions, no OAuth.
2. **No second authorisation path.** The HTTP API authorises by
   re-running `authorises(grants, caller, needed, target, now)` — the
   exact path the BEP dispatcher runs. A handler must never consult
   the verified claims, the requested scope, or the session class
   without also calling `authorises`.
3. **No silent widening on data verbs.** A `data:read` / `data:write`
   capability is folder-scoped at write time and at read time. The
   F1 namespace fix binds the stored scope to the canonical BEP
   folder id; the F4 fix blocks node-wide data grants and tokens.
   Both bars are enforced in the HTTP layer at the same points the
   BEP / CLI paths enforce them.
4. **No client-side trust.** The PWA never derives an authorisation
   decision from the `abilities` view. The server re-checks every
   request; `abilities` is a UI hint.
5. **No `unsafe`, no magic numbers, no silent fallbacks.** The Rust
   workspace rules apply: edition 2024, strict lints, no `?? ""` /
   `?? []` / `?? {}` to mask absence.
6. **No `serde(rename = "...")` per field.** Every schema uses
   `rename_all = "snake_case"`; per-field renames defeat the
   contract test.
7. **No wildcard CORS.** `cors_origins` cannot contain `*`. The
   config parser refuses it; the runtime never sees it.
8. **No endpoints outside `/v1`.** v1 is the entire surface; v2
   mounts at `/v2` and the v1 routes stay frozen.
9. **No CORS credentials.** `Access-Control-Allow-Credentials` is
   always `false`.
10. **No streaming except `/v1/folders/{folder}/archive`.** Every
    other route is a single request / single response.
11. **No management commands on the data plane without a
    `data:read` / `data:write` claim.** The data-plane gate is
    unchanged from the BEP path; the HTTP layer is a peer, not a
    bypass.

## F1–F4 compatibility

The HTTP API is compatible with the F1–F4 fix at every layer:

- **F1 (namespace fix).** `POST /v1/shares` and `POST /v1/grants`
  resolve the operator-facing folder name to `p2p-<name>` before
  storing the `Scope::folder` value, going through the same
  resolution function `cascade share add` and `cascade grant add`
  use. Unknown or non-P2P folders return `422 unprocessable` with
  `details.folders_known`.
- **F2 (explicit-control bit).** The data-plane gate consults the
  `data_explicit_control` table on every `data_access` call. The
  HTTP layer does not gate the data plane differently from the BEP
  layer; every `GET /v1/files/...` and `PUT /v1/files/...` flows
  through the same gate. `GET /v1/peers` surfaces the table so the
  PWA can render the explicit-control badge per peer per folder.
- **F3 (startup window).** `/v1/ready` reports the
  `data_plane_ready` bit. The HTTP server is bound before the bit
  flips (so readiness queries work from t=0), but every data-plane
  route (`/v1/files/...`, `/v1/folders/{folder}/archive`) returns
  `503 unavailable` with `code: "data_plane_not_ready"` while the
  bit is false. Management routes are unaffected.
- **F4 (no node-wide data verbs).** `POST /v1/shares`,
  `POST /v1/grants`, and `POST /v1/tokens` refuse `data:read` or
  `data:write` over `{ "kind": "node" }` with `422 unprocessable`
  and `code: "data_verb_node_wide_forbidden"`. The error names the
  offending capability, the offending scope, and the fix.

Data verbs are the only capability names that require this special
handling in the HTTP layer — the same set `Capability::is_data_verb()`
returns `true` for.

## Contract tests

The crate ships a `tests/contract.rs` integration test that:

- Spins up a real `Engine` over a tempdir with no backends.
- Stands up the `axum::Router` against the in-memory state.
- Issues a `CapabilityToken` signed by the node's own device identity
  for each (capability, scope) combination the v1 routes require.
- Walks the route table, asserting:
  - The HTTP status code matches the documented mapping for success.
  - The response body parses against the typed Rust schema.
  - The error envelope matches the typed `ApiError` schema for the
    documented failure cases (`unauthorised`, `forbidden`,
    `not_found`, `data_plane_not_ready`, `data_verb_node_wide_forbidden`,
    `delegation_exceeds_parent`).
  - The `X-Cascade-Request-Id` response header is present and
    26-char base32.

The contract test is the single source of truth for the wire shape. A
change to any schema that does not update the test fails CI.

## Open questions for the implementer

- **Process model.** The HTTP server runs in the same `tokio`
  runtime the daemon already uses — a second `axum::serve` task
  alongside the existing `presenter` task.
- **Auth rate limiting.** v1 does not ship a token-bucket on auth
  failures. The v1 cut assumes loopback bind (or a reverse proxy
  that rate-limits); a public bind with no rate limiter is a DoS
  surface. v2 adds it.
- **Body limits.** `axum`'s default 2 MiB body limit is overridden
  by `[web].max_body_bytes`. The crate's README is honest about
  both numbers so deployments that need > 1 GiB PUTs know the knob.
- **Session card in the PWA.** `GET /v1/session` is denormalised
  for first-load rendering. The PWA re-renders the abilities after
  every mutation that might change them (a `POST /v1/shares`, a
  `POST /v1/tokens/revoke`) rather than caching across mutations.

The implementer picks up from here. The contract is the spec; the
crate ships when the contract tests pass.
