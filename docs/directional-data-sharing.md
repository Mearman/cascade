# Directional data sharing

Two people, two nodes, one shared folder. How much does each side see? Cascade gives you a precise answer per-peer, per-folder, with no new access-control system and no change to the default for anyone who has not configured anything.

---

## The model

Every folder relationship between two peers has two independent dimensions:

- **may-they-read** — whether we serve this peer our index (the file listings) and our blocks (the actual content). Data flows *out* from us to them.
- **may-they-write** — whether we accept this peer's index and blocks, merging their changes into our local state. Data flows *in* from them to us.

Each direction is a yes or no. The combinations give four postures:

| Posture | may-they-read | may-they-write | What it means |
|---------|---------------|----------------|---------------|
| read-only | yes | no | The peer mirrors your folder; they cannot change yours |
| write-only | no | yes | A drop or backup sink; they push to you, learn nothing of what you hold |
| read-write | yes | yes | Full bidirectional sync — the default for a trusted peer |
| no-share | no | no | Trusted for transport but no folder data exchanged |

The language is always from *your* node's point of view. A read-only share *you* grant to a friend means *they* can read from *you*.

---

## Two friends, one shared folder: a worked example

Alice and Bob each run a Cascade node. Alice's device id is `ALICE-XXXX`, Bob's is `BOB-YYYY`. They want to share `/work/photos` — Alice's photos, which Bob can look at but not modify.

**On Alice's node:**

```
cascade share add BOB-YYYY /work/photos --direction read-only
```

This writes a `data:read` grant: Bob may read Alice's index and blocks for `/work/photos`. Because no `data:write` grant exists for Bob over that folder, Bob cannot push changes back — the decision is made automatically from the grant set. Alice sees:

```
Granted data:read to BOB-YYYY over /work/photos (expires never) [grant 1]
Sharing posture for BOB-YYYY over /work/photos: read-only
```

Verify the current state at any time:

```
cascade share list /work/photos
```

```
Directional data shares:
  BOB-YYYY over /work/photos: read-only
```

**On Bob's node:** Bob does not need to run any command. His node syncs against Alice's. From his side the folder behaves like any other read-write peer — he can read and push changes — but any changes he pushes will be rejected by Alice's node (they go into Alice's receive quarantine, described below). If Bob wants the relationship to be symmetric — that is, Bob should also serve Alice read-only — Bob runs the mirror command on his own node.

**To give Bob write access later:**

```
cascade share add BOB-YYYY /work/photos --direction write-only
```

This adds a `data:write` grant. The posture for Bob over `/work/photos` is now read-write (both grants present).

**To make it time-limited** (for example, a collaborator with temporary access):

```
cascade share add BOB-YYYY /work/photos --direction read-only --expires 2026-12-31T00:00:00Z
```

When the grant expires, the read direction lapses. Because any data grant covering the folder opts the peer into explicit directional control, an expired `data:read` grant with no active replacement means Bob's read direction is denied — it does not silently revert to the full-trust default until all data grants for that peer and folder are gone.

**To revoke the share entirely:**

```
cascade share revoke BOB-YYYY /work/photos
```

```
Revoked data:read for BOB-YYYY over /work/photos.
Revoked 1 grant(s). BOB-YYYY returns to the trusted-peer default (full sharing).
```

To revoke only one direction:

```
cascade share revoke BOB-YYYY /work/photos --direction read-only
```

---

## The default: no configuration, no change

A trusted peer in your peer list with no directional share configured keeps the full read-write behaviour that exists today. The feature only ever narrows — you configure a `data:read` or `data:write` grant to *restrict* a peer to one direction. A node that never runs `cascade share` behaves exactly as before.

Revoking the last data grant for a peer and folder returns them to this default. The peer is trusted for transport; both directions are open again.

---

## How the decision is made (the grant lookup)

The decision is made per frame, at the BEP session level, not at connection time. When Alice's node receives a frame from Bob, it asks: does Bob have a live data grant for this folder?

1. If Bob has an unexpired `data:read` grant covering the folder, the read direction is allowed for this frame.
2. If Bob has an unexpired `data:write` grant covering the folder, the write direction is allowed for this frame.
3. If Bob has *any* data grant at all for this folder — including only an expired one, or only the grant for the other direction — then the absent or lapsed direction is denied. Having any data grant opts the peer into explicit directional control for that folder.
4. If Bob has no data grant of any verb covering the folder, both directions are allowed. This is the trusted-peer default.

This is a default-open decision, the opposite of the management plane (which is default-closed). The grant set and the revocation list are consulted on every evaluation, so a revoked or expired grant stops working at the next frame — not at next restart.

---

## Capability tokens for portable credentials

The same signed-token machinery used for remote administration works for data sharing. Alice can mint a portable credential for Bob to present:

```
cascade token issue BOB-YYYY data:read /work/photos 2026-12-31T00:00:00Z
```

This prints a JSON token file that Alice can send to Bob out-of-band. Bob places the file somewhere on disk and presents it when his node connects to Alice's:

```
cascade remote ALICE-XXXX --token /path/to/token.json
```

The token is carried on the initial `ClusterConfig` frame of the BEP session (the `data_token` field). Alice's node verifies the signature, the expiry, the revocation list, and that the bearer matches the authenticated connection, then folds the carried grant into the access decision for the session. A token that fails verification is silently ignored — it cannot widen access, and a bad token is not an error because the data path is default-open.

Tokens can be delegated. Bob can mint a sub-token from his token for a third device, but the child's scope and expiry must be within the parent's — delegation cannot widen authority, and a child token never outlives the parent it derived from. The chain depth is capped at eight hops.

Alice can revoke a token before it expires:

```
cascade token revoke <token-id>
```

The revocation takes effect at the next frame evaluation — the revocation list is checked on every decision.

---

## What happens when a write is rejected (receive-only semantics)

If a peer tries to push a change to a folder where they do not have `data:write` — because the share is read-only, because their grant expired, or because it was revoked — their proposed file rows are not silently discarded. Following Syncthing's "local additions" model, the rejected rows are written to a quarantine table keyed by `(folder, peer, path)`, holding the proposed file record, and surfaced to the operator as "N rejected local additions from <peer>".

Quarantined rows are never merged into the live index, never block-fetched, and never re-advertised. A newer proposal for the same path from the same peer replaces the older one, so the quarantine stays bounded. The session stays up — the rejection does not break the connection.

If you later grant `data:write` for that peer and folder, their node re-sends its index on the next sync exchange, and the rows become eligible to merge then. Quarantined rows are not replayed automatically, so a stale proposal cannot resurrect content that was since deleted.

---

## What a write-only (drop/backup sink) relationship looks like

When `data:write` is granted but `data:read` is not, the peer is a write-only or drop/backup sink. They push their content in and we merge it; we serve them an empty index and empty block responses, so they learn nothing of what else the folder holds.

Our own local changes are simply never advertised to that peer. The peer, seeing an empty index from us, treats everything it holds as a local addition on its side — which is the symmetric, correct outcome and requires no special handling on ours.

---

## Security model

**Capabilities are signed.** A `data:read` or `data:write` grant in the on-node grants table is stored like any other grant and is evaluated by the same authorisation logic. A capability token is signed by the issuing node's real device-identity private key — the key behind its TLS certificate. A peer who knows only the public device id cannot forge a token.

**Delegation cannot escalate.** A delegated sub-token's capability must equal the parent's, its scope must be covered by the parent's, and its expiry must not exceed the parent's. Every hop in a chain can only narrow authority, never widen it.

**Revocation is prompt.** Both on-node grants and tokens are checked on every frame evaluation. A revoked token stops working at the next BEP frame, not at next restart. An expired grant narrows the decision the instant the clock passes the expiry.

**Unauthenticated sessions are denied.** When directional enforcement is active, a session whose peer identity was not established by an end-to-end TLS handshake — a relayed or post-hole-punch session where the device id is merely asserted rather than proven — is treated as no-share in both directions for any folder under directional control. The full-trust default applies only to TLS-verified peers.

**No parallel ACL system.** There is no separate access-control table. The access decision is the existing grants table (introduced for the management plane in schema v2) plus the token revocation list (schema v3), filtered to the two data verbs. Adding directional sharing to a node requires no migration and no schema change to existing tables. The only new storage is the `data_receive_quarantine` table (schema v4), which records rejected incoming file rows.

---

## Command reference

```
cascade share add <peer-device-id> <folder> --direction <read-only|write-only|read-write>
  [--expires <RFC 3339>]

    Grant a peer the specified sharing direction over a folder. read-only writes
    a data:read grant; write-only writes a data:write grant; read-write writes
    both. The audit trail, granted_by stamping, and storage are identical to
    cascade grant add.

cascade share list [<folder>]

    List all peers with a directional share, shown as read-only / write-only /
    read-write rather than raw capability rows. Pass a folder to filter.

cascade share revoke <peer-device-id> <folder> [--direction <read-only|write-only|read-write>]

    Remove data grants for the peer over the folder. Without --direction, all
    data grants for that peer and folder are removed, returning the peer to the
    trusted-peer default (full sharing while trusted). With --direction, only the
    grants for the named direction are removed.

cascade token issue <bearer-device-id> <data:read|data:write> <folder> <RFC 3339 expiry>

    Mint a portable signed credential for the bearer to present. The token is
    carried on the ClusterConfig frame of the BEP session. No new command is
    needed: data:read and data:write are ordinary capabilities the existing token
    machinery already mints, delegates, and revokes.

cascade token revoke <token-id>

    Add the token to the append-only revocation list. Takes effect at the next
    frame evaluation.
```

---

## Storage

`grants` (schema v2): no change. `data:read` and `data:write` are stored in the existing `capability` column and parsed back by `Capability::from_wire`. Existing rows are unaffected.

`capability_tokens` / `token_revocations` (schema v3): no change. A data-verb token is an ordinary token row.

`data_receive_quarantine` (schema v4): new table added for receive-only local additions.

```sql
CREATE TABLE data_receive_quarantine (
    folder_id     TEXT NOT NULL,
    peer_device   TEXT NOT NULL,
    path          TEXT NOT NULL,
    file_json     TEXT NOT NULL,   -- serialised proposed FileInfo
    observed_at   INTEGER NOT NULL, -- Unix seconds
    PRIMARY KEY (folder_id, peer_device, path)
);
```

The `trusted` allowlist (the peer list) is unchanged. Directional sharing is a refinement within the trusted set — a peer must be trusted (TLS-authenticated) before any data grant is consulted. Directional sharing does not replace the peer list; it narrows what a listed peer may do.

---

## Open questions for a future revision

**Explicit no-share marker.** Revoking the last data grant for a peer and folder returns them to the full-trust default while they remain in the peer list. An operator who wants "trusted for transport but no folder data exchanged" has no row that expresses that under the default-open model. The current design assumes that "trusted but no data" means removing the peer from `trusted` (option c); if a dedicated no-share posture is needed without removing the peer, a sentinel capability or a separate flag will be required. This was not resolved in v1.

**Token placement on the wire.** The data-verb token is carried on the optional `data_token` field of `BepMessage::ClusterConfig` — the first frame each side sends — rather than on a dedicated data-authorisation handshake frame. This was chosen for simplicity: the management plane carries its token on `ManageRequest`; carrying the data token on `ClusterConfig` means the serving side has it before it serves anything. A dedicated frame was considered but not adopted.
