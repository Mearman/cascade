// API types for the Cascade daemon v1 HTTP interface.
// Kept in lockstep with crates/cascade-web-api/src/schemas/ by hand review.

export type SessionClass = 'owner' | 'named_user' | 'bearer';

export type Capability =
  | 'status:read'
  | 'pin:write'
  | 'cache:manage'
  | 'config:push'
  | 'policy:set'
  | 'backend:manage'
  | 'lifecycle:control'
  | 'grant:admin'
  | 'data:read'
  | 'data:write';

export type Scope = { kind: 'node' } | { kind: 'folder'; path: string };

// CapabilityToken: the JSON the user pastes into the login form.
export interface CapabilityToken {
  token_id: string;
  issuer: string;
  bearer: string;
  capability: Capability;
  scope: Scope;
  expires: string; // RFC 3339
  issued_at: string; // RFC 3339
}

export interface Abilities {
  status_read: boolean;
  pin_write: boolean;
  cache_manage: boolean;
  config_push: boolean;
  policy_set: boolean;
  backend_manage: boolean;
  lifecycle_control: boolean;
  grant_admin: boolean;
  data_read: string[]; // folder prefixes (canonical BEP folder ids)
  data_write: string[]; // folder prefixes
}

export interface SessionInfo {
  class: SessionClass;
  node_device_id: string;
  verified_bearer: string;
}

export interface SessionResponse {
  session: SessionInfo;
  token: CapabilityToken;
  abilities: Abilities;
}

// ─── Health / Readiness ───────────────────────────────────────────────────────

export interface HealthResponse {
  status: 'ok';
  version: string;
  node_device_id: string;
}

export interface BackendEntry {
  id: string;
  name: string;
  folder_id: string | null; // canonical BEP id; null for non-P2P backends
  mount_path: string;
  healthy: boolean;
}

export interface BackendsResponse {
  backends: BackendEntry[];
}

export interface ReadyResponse {
  ready: boolean;
  data_plane_ready: boolean;
  backends: BackendEntry[];
  started_at: string; // RFC 3339
}

// ─── Files / Folders ─────────────────────────────────────────────────────────

export type EntryKind = 'file' | 'directory';

export interface FileEntry {
  name: string;
  kind: EntryKind;
  size: number | null; // null for directories
  mtime: string | null; // RFC 3339
  etag: string | null;
}

export interface FolderChildrenResponse {
  folder: string;
  path: string;
  entries: FileEntry[];
  next_cursor: string | null;
}

export interface CreateDirBody {
  name?: string;
}

export interface MoveEntryBody {
  from: string;
  to: string;
}

export interface EntryMetaResponse {
  name: string;
  kind: EntryKind;
  size: number | null;
  mtime: string | null;
  etag: string | null;
}

// ─── Shares ──────────────────────────────────────────────────────────────────

export type SharePosture = 'read-only' | 'write-only' | 'read-write';

export interface ShareEntry {
  peer_device_id: string;
  folder: string;
  folder_id: string;
  posture: SharePosture;
  granted_by: string;
  expires: string | null; // RFC 3339
  grant_ids: number[];
}

export interface SharesResponse {
  shares: ShareEntry[];
}

export interface CreateShareBody {
  peer_device_id: string;
  folder: string;
  posture: SharePosture;
  expires?: string; // RFC 3339
}

// ─── Tokens ──────────────────────────────────────────────────────────────────

export interface TokenEntry {
  token_id: string;
  bearer: string;
  capability: Capability;
  scope: Scope;
  expires: string; // RFC 3339
  issued_at: string; // RFC 3339
  revoked: boolean;
}

export interface TokensResponse {
  tokens: TokenEntry[];
}

export interface RevokeTokenResponse {
  revoked_at: string; // RFC 3339
}

export interface CreateTokenBody {
  bearer: string;
  capability: Capability;
  scope: Scope;
  expires: string; // RFC 3339
}

// ─── Grants ──────────────────────────────────────────────────────────────────

export interface GrantEntry {
  id: number;
  grantee: string; // device id
  capability: Capability;
  scope: Scope;
  expires: string | null; // RFC 3339
  granted_by: string;
  granted_at: string; // RFC 3339
}

export interface GrantsResponse {
  grants: GrantEntry[];
}

export interface CreateGrantBody {
  grantee: string;
  capability: Capability;
  scope: Scope;
  expires?: string; // RFC 3339
}

// ─── Audit ───────────────────────────────────────────────────────────────────

export interface AuditEntry {
  id: number;
  timestamp: string; // RFC 3339
  actor_device: string;
  capability: string;
  scope: Scope;
  command: string;
  outcome: string;
  request_id: string | null;
}

export interface AuditResponse {
  entries: AuditEntry[];
  next_cursor: string | null;
}

// ─── Peers ───────────────────────────────────────────────────────────────────

export interface PeerEntry {
  device_id: string;
  name: string | null;
  online: boolean;
  last_seen_at: string | null;
  data_verb_grants: Record<string, SharePosture>;
  explicit_control: string[];
}

export interface PeersResponse {
  peers: PeerEntry[];
}

// ─── Pins ────────────────────────────────────────────────────────────────────

export interface PinEntry {
  id: number;
  path_glob: string;
  folder: string;
  folder_id: string;
  created_at: string; // RFC 3339
}

export interface PinsResponse {
  pins: PinEntry[];
}

export interface CreatePinBody {
  path_glob: string;
  folder: string;
}

// ─── Policies ────────────────────────────────────────────────────────────────

export interface PolicyEntry {
  id: number;
  folder: string;
  folder_id: string;
  rule: string;
  created_at: string; // RFC 3339
}

export interface PoliciesResponse {
  policies: PolicyEntry[];
}

export interface CreatePolicyBody {
  folder: string;
  rule: string;
}

// ─── Cache ───────────────────────────────────────────────────────────────────

export interface CacheWarmBody {
  path_glob: string;
}

export interface CacheActionResponse {
  summary: string;
}

// ─── Config ──────────────────────────────────────────────────────────────────

export type ConfigFormat = 'gitignore' | 'toml' | 'yaml' | 'json';

export interface ConfigPushBody {
  folder: string;
  format: ConfigFormat;
  body: string;
}

// ─── Pagination ──────────────────────────────────────────────────────────────

export interface PaginationParams {
  limit?: number;
  cursor?: string;
}

// ─── API Error ───────────────────────────────────────────────────────────────

export type ErrorCode =
  | 'unauthorised'
  | 'forbidden'
  | 'not_found'
  | 'conflict'
  | 'gone'
  | 'payload_too_large'
  | 'unprocessable'
  | 'rate_limited'
  | 'internal'
  | 'unavailable'
  | 'timeout'
  | 'bearer_mismatch'
  | 'token_too_large'
  | 'chain_too_deep'
  | 'data_verb_node_wide_forbidden'
  | 'delegation_exceeds_parent'
  | 'data_plane_not_ready'
  | 'precondition_failed';

export interface ApiErrorDetail {
  code: ErrorCode;
  message: string;
  request_id: string;
  details?: Record<string, unknown>;
}

export interface ApiErrorResponse {
  error: ApiErrorDetail;
}

export class ApiError extends Error {
  readonly code: ErrorCode;
  readonly request_id: string;
  readonly details: Record<string, unknown> | undefined;

  constructor(detail: ApiErrorDetail) {
    super(detail.message);
    this.name = 'ApiError';
    this.code = detail.code;
    this.request_id = detail.request_id;
    this.details = detail.details;
  }
}
