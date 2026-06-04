# Cascade PWA v2 — Rust + WASM core

The v1 PWA is a Preact + TypeScript application that drives the daemon over
the JSON HTTP API in `docs/api-contract.md`. The v2 PWA moves the security-
sensitive core — device identity, capability-token signing and verification,
end-to-end block encryption, request/response schemas — into a Rust crate
compiled to `wasm32-unknown-unknown` and embedded in the same PWA bundle.
The TypeScript layer keeps the UI, the fetch glue, and any non-security-
critical code.

This document is the v2 design. v1 is the implementation that runs today;
the migration does not fork the codebase at any point. v1 reads this
document and the API contract; the design decisions v1 must respect are
listed at the end.

British English. No emojis. No magic numbers. The workspace lints (edition
2024, pedantic + nursery + cargo clippy denied, `unwrap_used` /
`expect_used` / `indexing_slicing` / `string_slice` / `unsafe_code`
denied) apply to every new crate and to the WASM build in the same way
they apply to the native build. The full v2 cut is a multi-milestone
migration; the numbered sequence at the end is the sequence.

## Goal

A single Rust workspace builds three artefacts from the same source tree:

1. The `cascade` daemon (native, today).
2. The PWA's TypeScript shell (Vite, today).
3. A `cascade-pwa-core` WASM module (new, in v2) that the PWA embeds.

The PWA in v2 holds the device identity in the browser's IndexedDB, signs
capability tokens locally, verifies them locally, and encrypts block
content before it crosses the network. The daemon's role collapses to
"authoritative state + relay": the data-plane gate still calls
`authorises(grants, caller, needed, target, now)` exactly as the BEP
dispatcher does, but the cryptography and the schema validation live in
the WASM module both sides import.

The boundary is sharp: the WASM module is a pure library. No DOM. No
`fetch`. No `WebSocket`. The TypeScript layer owns every browser API and
hands serialised data — JSON strings, `Uint8Array` views, opaque base64
blobs — to the WASM module as function arguments. The WASM module returns
the same kinds of value. The PWA never trusts a string the WASM module
did not produce; the daemon never trusts a claim the WASM module did not
verify.

## Crate selection

The Rust workspace already contains one crate that compiles cleanly to
`wasm32-unknown-unknown`: `cascade-announce-wire` (see
`crates/cascade-announce-wire/Cargo.toml` and the `cascade-announce-worker`
WebAssembly build under `workers/announce/`). The pattern is the
reference for the v2 work: serde + sha2 + hmac + ed25519-dalek +
data-encoding, no tokio, no reqwest, no rustls, no rusqlite, no `ring`,
no `rcgen`, no libc.

Three categories of crate emerge.

### Compiles to `wasm32-unknown-unknown` with no changes (or minor cfg work)

| Crate | What it provides | Action |
|-------|------------------|--------|
| `cascade-announce-wire` | Wire types, ed25519 seed derivation, signed-candidate envelope, HMAC client auth, announce handler. | Already wasm-safe. The v2 PWA reuses `signing::SignedCandidates` and `auth` for peer-to-peer envelopes. |
| `cascade-config` | `.cascade` parser (four formats), merge, directory walk, typed `CascadeConfig`. Pure serde + toml + serde_yaml + serde_json. | Compiles to wasm. The PWA uses it to render the config editor and to parse a user-pasted `.cascade` file in the browser before pushing it to the daemon. |
| `cascade-expr` | PEG parser and evaluator for the conditional expression language. | Compiles to wasm after cfg-gating the `libc` dependency (only `DiskProvider` uses it; the wasm build does not need disk providers). The PWA evaluates `.cascade` conditions against the `EvalContext` it constructs from the device and the daemon's reported state. |

The cfg-gating pattern follows the workspace convention documented in
`Cargo.toml`: split the function into two cfg-gated definitions, one per
target, rather than `#[allow]`-ing the lint. For `libc`, the wasm build
drops the `DiskProvider` and the `process::exit` reachability entirely;
the parser and evaluator are pure data and run unchanged.

### Needs a `wasm-bindgen` façade

A new crate, `cascade-pwa-core`, sits in the workspace at
`crates/cascade-pwa-core/`. It depends on `cascade-announce-wire`,
`cascade-config`, `cascade-expr`, and a small set of pure-Rust crypto
crates (`ed25519-dalek`, `sha2`, `hmac`, `chacha20poly1305`,
`x25519-dalek`, `getrandom` with the `wasm-bindgen` feature for
seeding). It re-exports the existing logic behind a `#[wasm_bindgen]`
surface and adds the v2-only logic that the PWA needs:

- Device identity lifecycle: `generate()`, `from_pkcs8(bytes)`,
  `to_pkcs8()`, `device_id()`, `signing_key_bytes()`,
  `verifying_key_bytes()`, `certificate_der()`. The PWA stores the
  PKCS#8-encoded private key in IndexedDB (the same bytes the daemon
  stores on disk); the WASM module never touches IndexedDB itself.
- Capability-token ergonomics: `mint_token(claims_json,
  private_key_bytes, now)`, `verify_token(token_json,
  trusted_issuer_keys, now)`, `delegate_token(parent_json,
  child_claims_json, now)`. The WASM module is the canonical
  serializer; the daemon re-verifies the bytes the PWA produced.
- Block encryption: `encrypt_block(plaintext, key, aad) ->
  ciphertext`, `decrypt_block(ciphertext, key, aad) -> plaintext`.
  XChaCha20-Poly1305, 192-bit nonce, AAD carries the folder id and
  the block hash. The PWA encrypts before a `PUT /v1/files/...` and
  decrypts after a `GET`; the daemon stores ciphertext only when the
  PWA marks the request end-to-end, otherwise it stores plaintext
  via the existing backend path.
- Schema validation: `validate_session(json) -> Result<...>`,
  `validate_share(json) -> Result<...>`, etc. The validators mirror
  the daemon's request-handler preconditions (F1 namespace fix, F2
  explicit-control bit, F3 readiness bit, F4 no-node-wide-data-verb
  bar) and are called before the PWA even sends the request. A
  pre-flight failure in the browser is a fast user-visible error
  rather than a `422` round-trip; the daemon's own check remains the
  security boundary.

The façade is thin. It does not implement the protocol, hold state, or
schedule work. The TypeScript layer constructs the input JSON, calls
one WASM function, and acts on the result. The WASM module is a
deterministic function of its inputs and the embedded keys.

### Stays Rust-only

| Crate | Why |
|-------|-----|
| `cascade-p2p` | tokio + rustls + ring + rcgen + tokio-tungstenite + mainline DHT + reqwest. The connectivity stack is fundamentally a native-runtime concern. |
| `cascade-engine` | rusqlite + cascade-p2p + cascade-config. The state database, the VFS tree, the cache manager, and the manage dispatcher all run on the daemon. |
| `backend-gdrive`, `backend-s3`, `backend-local`, `backend-p2p` | All require a native HTTP client and (for `backend-p2p`) a block store on disk. The PWA does not run backends. |
| `presenter-nfs`, `presenter-fuse`, `presenter-fileprovider`, `presenter-fskit`, `presenter-projfs`, `presenter-webdav` | All are OS filesystem surfaces. The PWA is one such surface — the WebDAV presenter could in principle serve the PWA's content over WebDAV, but that is a deployment concern, not a WASM concern. |
| `relay-server` | Binary Cloudflare-style relay for NAT traversal. |

The rule of thumb: anything that depends on tokio's reactor, on
`std::fs`, on `std::net`, on a TLS stack, or on a SQLite database, does
not enter the WASM build. The split is enforced by `cascade-pwa-core`'s
own `Cargo.toml`: it lists only the crates in the first two tables.

## JS↔WASM boundary and cost model

The boundary cost is not free. Each WASM call crosses the
JS↔WASM line once, marshals its arguments, and unmarshals its return
value. The PWA's request budget is one call per logical operation;
chaining a dozen WASM calls inside one user action (for example,
"compose a token, encrypt a block, build the request body") is
acceptable; chaining a WASM call per byte of a 1 MiB file is not.

Three concrete conventions:

1. **One call per logical operation.** The façade exposes coarse
   operations (`mint_token`, `verify_token`, `encrypt_block`,
   `decrypt_block`, `validate_session`, `validate_share`). Each call
   takes a JSON string and returns a JSON string or a
   `Uint8Array`. The TypeScript layer never breaks one of these
   into multiple WASM calls because the boundary cost is wasted.
2. **Strings cross as JSON, not as parsed objects.** A `String` in
   WASM is a `string` in JavaScript. Passing a string across the
   boundary copies it; passing a parsed object adds a serialise +
   parse round trip the PWA does not need. The WASM module
   `serde_json::from_str` on the way in and `to_string` on the way
   out, and returns the result as a `String`. The TypeScript layer
   `JSON.parse`s once.
3. **Bytes cross as `Uint8Array`, never as base64.** `Vec<u8>` in
   WASM is a `Uint8Array` in JavaScript. The `wasm-bindgen` glue
   does not copy the underlying buffer for a typed-array view; the
   PWA can hand the WASM module the `Uint8Array` it got from
   `fetch().then(r => r.arrayBuffer())` without a copy, and the
   WASM module returns a `Uint8Array` the PWA can put straight into
   a `fetch` body. Bulk data — block content, encrypted
   directories, the PKCS#8 key blob — uses this path. Opaque blobs
   (a base64-encoded token in a JSON field) cross as `string`.

The cost model is summarised in the table below. Numbers are
ballpark for a modern laptop; the design is correct for any
plausible number, but the shape of the budget is the load-bearing
part.

| Path | Round-trip cost | Budget |
|------|-----------------|--------|
| `mint_token(claims_json)` | one call, two string copies | < 200 µs |
| `verify_token(token_json)` | one call, two string copies, one ed25519 verify | < 1 ms |
| `encrypt_block(plaintext)` | one call, one `Uint8Array` view | 1 MiB / ~50 ms (XChaCha20-Poly1305) |
| `decrypt_block(ciphertext)` | one call, one `Uint8Array` view | 1 MiB / ~50 ms |
| `validate_session(json)` | one call, one schema check | < 50 µs |

The PWA never crosses the boundary inside a render. The render path
holds a `SessionResponse` parsed once from the last successful call;
mutations (a `POST /v1/shares`, a `POST /v1/tokens/revoke`) trigger a
single `validate_*` call before the fetch and a single
`verify_token` call after, not a per-keystroke validation.

## Crypto in the browser

The v1 PWA does no crypto. It carries a `CapabilityToken` JSON
document in the `Authorization` header and lets the daemon do
everything. v2 moves the cryptographic operations into the browser
because the v1 model is round-trip-heavy: every token issuance is a
`POST /v1/tokens`, every verification is implicit in the daemon's
response, and there is no way to validate a token offline (for
example, before pasting it into a teammate's `cascade remote`).

The primitives the v2 WASM module exposes are the primitives the
cryptography needs, no more and no fewer:

- **Ed25519 signing and verification** for capability tokens and
  for the BEP-style signed-candidate envelopes the announce and
  DHT paths already use. `ed25519-dalek` is pure Rust and
  compiles to wasm32-unknown-unknown without changes; the
  workspace already pins it for `cascade-announce-wire`. The PWA
  holds the private half in IndexedDB; the public half is the
  device id and travels with every token.
- **X25519 key agreement and XChaCha20-Poly1305 symmetric
  encryption** for end-to-end block encryption. `x25519-dalek`
  and `chacha20poly1305` are pure Rust and wasm-clean. The PWA
  derives a per-peer shared secret from its X25519 private key
  and the peer's published X25519 public key (carried in the
  peer's `data_explicit_control` row, see F2), HKDF-expands it
  into a per-folder subkey, and uses that subkey as the symmetric
  key for `encrypt_block` and `decrypt_block`.
- **SHA-256** for content addressing (block hashes) and for the
  etag derivation. `sha2` is in the workspace already and is
  wasm-clean.
- **HMAC-SHA256** for the announce write-auth primitive the
  `cascade-announce-wire::auth` module already implements. The
  PWA does not run an announce server, but the same primitive is
  useful for client-side request signing if a future endpoint
  needs it; the WASM module exposes it.
- **Random bytes from `getrandom` with the `wasm-bindgen`
  feature.** The wasm-bindgen implementation routes through
  `crypto.getRandomValues` in the browser, which is the only
  acceptable entropy source. `ring` is not used; `ring`'s
  WebAssembly support is incomplete and its P-256 implementation
  is not what the WASM module needs anyway (the device identity
  in v2 is Ed25519, not ECDSA P-256 — see "Open questions").

The WASM module does not use `window.crypto.subtle` directly. The
reason is the boundary cost and the async surface area: every
`SubtleCrypto` call is a `Promise`, and a PWA-side `await` before
every WASM call adds latency the design does not budget for. The
pure-Rust primitives are also faster on small inputs (signing a
300-byte token is microseconds; the WebCrypto promise overhead is
milliseconds) and avoid the WebCrypto origin restrictions that
break the PWA in `file://` contexts and in cross-origin iframe
embeds.

## API client migration

The v1 PWA carries hand-maintained TypeScript types in
`apps/web/src/api/types.ts`, kept in lockstep with the Rust schemas
by review. The API contract document is explicit that the v1 choice
is hand-maintained types with `schemars` schemas emitted as
reference; a future codegen entry point is a v2 concern. v2 picks
the codegen tool and the v1 cut is shaped to make the switch a
mechanical search-and-replace.

Three options were considered:

1. **`specta`.** Strong for Tauri-style "expose Rust functions to
   JavaScript via `tauri-specta`", but its TypeScript output is
   geared toward Tauri command patterns, not toward a JSON HTTP
   API client. The PWA's `fetch` calls would not benefit.
2. **`ts-rs`.** Compile-time TypeScript generation via a
   `#[derive(ts_rs::TS)]` trait, writing `.ts` files next to the
   source at build time. The generated types are real TypeScript,
   not a JSON Schema document, so the PWA consumes them directly.
   `ts-rs` is dependency-light, has no runtime, and the
   generator is a Cargo build script the workspace already
   controls.
3. **`schemars` + `quicktype`.** `schemars` derives a JSON Schema
   at compile time; `quicktype` reads the JSON Schema and emits
   TypeScript. The intermediate JSON Schema is useful as
   documentation and as a wire contract snapshot, but the
   generator chain has more moving parts and `quicktype`'s output
   is harder to read than `ts-rs`'s.

The v2 choice is `ts-rs`. The reasoning:

- The PWA's TypeScript types are colocated with the Rust schemas
  in the same `cascade-web-api` crate. A change to a schema
  fails the build at the type-generation step rather than at a
  later drift check. There is no second source of truth.
- The output is real TypeScript. The PWA can put `readonly` on
  fields, brand the request types, and split union types into
  `kind`-discriminator narrowings without negotiating with a
  generator.
- The CI gate is `cargo test -p cascade-web-api` (which runs the
  build script and writes the `.ts` files into a stable path)
  plus a `git diff --exit-code` on the generated tree. The
  `cascade-web-api` crate publishes the generated types as a
  path dependency the PWA consumes; drift is a CI failure, not
  a code-review observation.
- The cost is one extra `#[derive(ts_rs::TS)]` per schema type
  alongside the existing `#[derive(Serialize, Deserialize)]`.
  v1 already emits the same Rust types; v2 adds the derive and
  the build script.

`specta` is rejected because the PWA's HTTP client does not match
Tauri's IPC model. `schemars` + `quicktype` is rejected because
the indirection through JSON Schema adds a tool to the toolchain
and a file format to the diff for no PWA-side benefit. The
generated types in v2 are the v1 `types.ts` with a different
provenance; the API contract's stability promise is unchanged.

## Migration sequence

The sequence takes the v1 PWA to the v2 PWA without a parallel
codebase at any point. Each step is shippable; the v1 PWA does
not break.

1. **Land the v1 PWA and the API contract.** The
   `apps/web/` package, the `crates/cascade-web-api/` daemon
   crate, the `cascade start --web` flag, and the v1 contract
   test are the v1 cut. No WASM work. The hand-maintained
   `apps/web/src/api/types.ts` is the v1 source of truth on the
   PWA side; the contract test is the source of truth on the
   daemon side.
2. **Add `ts-rs` to `cascade-web-api`'s dev-dependencies and a
   build script.** The build script generates
   `crates/cascade-web-api/ts/<resource>.ts` from every
   `#[derive(Serialize, Deserialize, TS)]` schema. The generated
   tree is `#[cfg(test)]`-gated and not in the daemon's release
   binary. The contract test continues to assert the wire shape;
   `ts-rs` derives the TypeScript projection from the same Rust
   types. Drift between the wire shape and the generated TS is
   impossible — they are the same source.
3. **Publish the generated TS into the PWA repo as a path
   dependency.** The PWA's `package.json` gains
   `"@cascade/web-api-types": "file:../crates/cascade-web-api/ts"`.
   The hand-maintained `apps/web/src/api/types.ts` is removed.
   The PWA's `import type { SessionResponse } from
   "@cascade/web-api-types"` replaces the local import. CI on
   the PWA side runs `cargo test -p cascade-web-api` first and
   fails if the generated tree has uncommitted changes.
4. **Extract `cascade-pwa-core` as a wasm-bindgen crate.** The
   new crate depends on `cascade-announce-wire`,
   `cascade-config`, and `cascade-expr`. It adds the device
   identity types, the `mint_token` / `verify_token` /
   `delegate_token` façade, the `encrypt_block` /
   `decrypt_block` façade, and the `validate_*` façade. The
   crate is compiled to `wasm32-unknown-unknown` by a separate
   `cargo build --target wasm32-unknown-unknown -p
   cascade-pwa-core` and the resulting `.wasm` plus the
   `wasm-bindgen`-generated `.js` glue is emitted to
   `apps/web/src/wasm/`. Vite picks them up as static assets.
5. **Wire the WASM module into the v1 PWA as opt-in.** The PWA
   gains a `core.init()` call at startup; if the WASM module
   fails to load (a service worker is intercepting the request,
   a Content-Security-Policy forbids `wasm-unsafe-eval`, the
   user is on a browser without WASM), the PWA falls back to
   the v1 daemon-round-trip model. The fallback is the same
   code path the v1 PWA used. The opt-in is a feature flag, not
   a parallel implementation.
6. **Move token verification client-side.** The PWA's session
   store calls `core.verify_token(token_json, [daemon_pubkey],
   now)` after every `GET /v1/session`. The verified claims are
   the UI state; the daemon's response is a hint, not the
   source of truth. The PWA's "abilities" view is now
   cryptographically grounded — the v1 contract's "no
   client-side trust" rule is satisfied by construction, not by
   convention.
7. **Move token issuance client-side.** The `POST /v1/tokens`
   route stays (the daemon is the authority for which tokens
   exist), but the PWA builds the claim set in the WASM module,
   signs the token locally, and submits the signed JSON. The
   daemon verifies the signature, checks the F4 no-node-wide
   bar, checks the F1 namespace fix, and either accepts the
   submitted JSON or rejects it. The PWA no longer needs to
   carry a private key into a `POST` body that traverses a
   reverse proxy.
8. **Move block encryption client-side.** The PWA's `PUT
   /v1/files/...` body is now `core.encrypt_block(plaintext,
   per_folder_key, aad)`. The PWA's `GET` response is
   `core.decrypt_block(ciphertext, per_folder_key, aad)`. The
   daemon stores ciphertext; the PWA derives the per-folder
   key from the verified peer's X25519 public key (carried in
   `data_explicit_control` per F2). End-to-end encryption is a
   v2 feature; the v1 path is unchanged for backends that
   store plaintext.
9. **Drop the fallback.** With the WASM module shipped and
   stable, the v1 round-trip model is removed. The v2 PWA
   requires WebAssembly and IndexedDB. Browsers that do not
   support them get a clear "browser not supported" screen
   rather than a degraded mode.

The sequence keeps every commit shippable. The diff between v1
and v2 is a series of independent, testable changes; there is no
"v2 fork" branch and no "v1 maintenance" branch. The v1 PWA at
step 5 is the v1 PWA plus a feature flag; the v2 PWA at step 9
is the same PWA with the fallback removed.

## v1 must NOT

The v1 PWA ships now. To keep the v2 migration a mechanical
sequence, v1 must respect the following design decisions.

1. **No TypeScript-side crypto.** No `crypto.subtle`, no
   `tweetnacl`, no `noble-ed25519`, no hand-rolled base64. The v1
   PWA carries tokens; the daemon signs and verifies. v2 moves
   this into the WASM module; v1 must not pre-empt it.
2. **No new credential format.** The HTTP API authenticates with
   the signed `CapabilityToken` from the BEP management plane.
   v1 must not introduce a bearer-without-signature path, an API
   key, a cookie, or a daemon-issued session id. The PWA's
   login flow pastes a token JSON; that is the only flow.
3. **No `serde_json::Value` for opaque data.** Every schema
   field in `cascade-web-api` is a typed Rust value. A claim
   the schema does not understand is `String` (with
   `#[ts(type = "string")]` if `ts-rs` would otherwise infer a
   wider type), not `serde_json::Value`. The PWA's TypeScript
   types are the projection of the Rust types; loose JSON is a
   second source of truth.
4. **No `HashMap<String, T>` for request bodies.** Every request
   body in `cascade-web-api` is a `#[derive(Deserialize)]` struct
   with named fields. The PWA's generated TS types are objects
   with named fields; an open-ended `Record<string, unknown>` is a
   step backward into the JSON Schema world.
5. **No per-field `serde(rename = "...")`.** Every schema uses
   `#[serde(rename_all = "snake_case")]`. The v2 `ts-rs`
   generator inherits the rename; per-field renames break the
   generation and force a hand-maintained override.
6. **No `any` in TypeScript.** The PWA's domain model is
   `unknown` plus type guards at the parse boundary. A typed
   function that takes `T` is a typed function; a function that
   takes `any` is a v1 wart that v2 will not inherit.
7. **No client-side trust of the `abilities` view.** The v1
   contract is explicit: the PWA renders the denormalised
   `SessionResponse.abilities` for the first load and after a
   mutation, but every authorisation decision the daemon makes
   is re-checked server-side. v2 makes this stronger (the PWA
   verifies the token cryptographically), not weaker; v1 must
   not pre-empt the cryptographic check by caching an
   authorisation decision client-side.
8. **No `cascade web` CLI subcommand, no `--web-manage`
   flag.** v1's daemon side is `cascade start --web --web-bind
   --web-bundle-url`. Adding more flags is a v1 surface that v2
   will not need. v2 collapses the surface into the existing
   flags plus the WASM module's configuration.
9. **No streaming except `/v1/folders/{folder}/archive`.** v1 is
   request / response only. v2 introduces SSE and WebSocket;
   the v1 PWA must not introduce a polling loop or a long-poll
   endpoint as a workaround for the v1 cut.
10. **No body-parser relaxation in the daemon.** `axum`'s
    default 2 MiB body limit is overridden only by
    `[web].max_body_bytes`. v1 must not introduce a
    per-route body limit negotiation; the v2 WASM module
    chunks block content on the PWA side and uses the existing
    PUT path.
11. **No `feature web` in `cascade` that pulls in `axum` for
    every build.** The daemon's `--web` flag is a
    feature-gated dependency so a daemon that never serves the
    PWA does not link `axum`. v1 must not unfeature-gate this;
    the WASM module in v2 is a separate build target with its
    own linker discipline.

The v1 PWA is the contract's source of truth on the client side.
A v1 commit that violates one of these decisions forces a v2
migration step to either re-shape the Rust schemas (a wire break
the API contract forbids) or carry a v1 wart forward into v2
(a piece of complexity the v2 cut does not need). Either is a
rework the sequence above is designed to avoid.

## Open questions

- **Device identity key type.** v1's device identity is ECDSA
  P-256 (the TLS-cert key, signed by `rcgen` and stored in
  `cascade-p2p/src/identity.rs`). v2's WASM module wants
  Ed25519 for the same reasons the announce wire already uses
  it (pure-Rust, wasm-clean, faster on small inputs). The
  cleanest path is to keep the TLS cert key as ECDSA P-256 (the
  P2P stack depends on it) and to derive a separate Ed25519
  signing key for capability tokens. The derivation (HKDF over
  the device cert's private key bytes) is what the announce
  wire already does for its ed25519 seed. The `cascade-pwa-core`
  crate carries the Ed25519 path; the TLS cert path stays
  in `cascade-p2p`. v1's `CapabilityToken` is signed by the
  ECDSA key today; v2's is signed by the Ed25519 key. The
  transition is a token-version bump (`TOKEN_SIGNING_DOMAIN`
  already includes `v1`; v2 introduces `v2`) and a
  `cascade-engine::manage::token::verify` branch.
- **IndexedDB schema migrations.** The v2 PWA holds the device
  identity's PKCS#8 private key in IndexedDB. A schema
  migration path (key rotation, recovery from a corrupted
  store, multi-device sync) is a v2 design that v1 does not
  pre-empt. The simplest v2 model is "one device identity per
  PWA install; key rotation is a fresh install". A
  multi-device model is a v3 concern.
- **Content-Security-Policy for `wasm-unsafe-eval`.** The WASM
  module requires `wasm-unsafe-eval` in the PWA's CSP. v1's
  deployment guide (if any) must include the CSP header the
  PWA expects; v2 does not relax the CSP. The PWA is a
  self-hosted static bundle; the operator controls the CSP.
- **IndexedDB encryption at rest.** The device identity's
  PKCS#8 bytes in IndexedDB are protected by the browser's
  same-origin policy, which is the same boundary that protects
  every other PWA secret. A passphrase-derived wrap key (the
  user types a passphrase at install, the WASM module derives
  a key with Argon2id, the IndexedDB value is the
  PKCS#8-wrapped-with-the-passphrase-key) is a v3 concern. v1
  and v2 leave the bytes in plaintext IndexedDB and rely on
  the browser sandbox.
- **Daemon-side WASM import.** The daemon can in principle
  import the same `cascade-pwa-core` crate to verify tokens
  and to encrypt blocks server-side, eliminating a class of
  "the client and the server disagree" bugs. v1 and v2 keep
  the daemon on the native crypto path; v3 unifies the two.
- **Streaming in v2.** SSE and WebSocket for live change feeds
  and sync progress are v2 features. The v1 PWA polls; the v2
  PWA subscribes. The wire shape (SSE event format,
  WebSocket subprotocol) is a v2 design that follows this
  document and the API contract's "v2 mounts at `/v2`" rule.

The v2 PWA is a multi-milestone migration. The v1 PWA is the
contract; the sequence above is the plan; the open questions
above are the items the implementer picks up after the v1 cut
lands.
