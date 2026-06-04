// API types for the Cascade daemon HTTP interface.
// These types mirror the Rust types served by the daemon when --web is enabled.

export type Role = 'owner' | 'named-user' | 'bearer';

export interface AuthToken {
  role: Role;
  // Owner and named-user tokens carry a device id.
  deviceId?: string;
  // Bearer tokens carry the issuer device id and the capability scope.
  issuerId?: string;
  scope?: string;
  expiresAt?: string; // RFC 3339
}

// ─── Status ───────────────────────────────────────────────────────────────────

export interface StatusResponse {
  version: string;
  uptimeSeconds: number;
  backends: BackendStatus[];
  cache: CacheStatus;
  peers: PeerStatus[];
}

export interface BackendStatus {
  id: string;
  displayName: string;
  mountPath: string;
  healthy: boolean;
  quota?: QuotaInfo;
}

export interface QuotaInfo {
  totalBytes: number | null;
  usedBytes: number | null;
  availableBytes: number | null;
}

export interface CacheStatus {
  totalBytes: number;
  usedBytes: number;
  pinnedBytes: number;
  cachedFiles: number;
  pinnedFiles: number;
}

export interface PeerStatus {
  deviceId: string;
  name: string;
  online: boolean;
  lastSeenAt: string | null;
  folders: string[];
}

// ─── Files / Tree ─────────────────────────────────────────────────────────────

export interface FileEntry {
  id: string;
  name: string;
  parentId: string | null;
  isDir: boolean;
  size: number | null;
  modTime: string | null; // RFC 3339
  mimeType: string | null;
  cacheState: CacheState;
}

export type CacheState = 'online' | 'cached' | 'pinned' | 'downloading';

// ─── Grants / Sharing ─────────────────────────────────────────────────────────

export interface GrantEntry {
  id: number;
  grantee: string; // device id
  capability: Capability;
  scopeKind: ScopeKind;
  scopePath: string;
  expiresAt: string | null;
  grantedBy: string;
  grantedAt: string;
}

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

export type ScopeKind = 'node' | 'folder';

export interface ShareEntry {
  peerId: string;
  folder: string;
  direction: 'read-only' | 'write-only' | 'read-write';
  expiresAt: string | null;
}

// ─── Token ────────────────────────────────────────────────────────────────────

export interface TokenEntry {
  id: string;
  bearerId: string;
  capability: Capability;
  scopeKind: ScopeKind;
  scopePath: string;
  expiresAt: string;
  issuedAt: string;
  parentId: string | null;
  revoked: boolean;
}

// ─── API Error ────────────────────────────────────────────────────────────────

export interface ApiError {
  error: string;
  detail?: string;
}
