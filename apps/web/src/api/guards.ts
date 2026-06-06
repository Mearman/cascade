// Runtime type guards for API response types.
// Each guard checks the minimal set of key fields needed to distinguish the
// response shape. This is not exhaustive schema validation — it provides basic
// runtime safety at the JSON boundary while satisfying the strict type-narrowing
// requirements of the ESLint `consistent-type-assertions: never` rule.

import type {
  SessionResponse,
  HealthResponse,
  ReadyResponse,
  FolderChildrenResponse,
  EntryMetaResponse,
  SharesResponse,
  ShareEntry,
  TokensResponse,
  RevokeTokenResponse,
  GrantsResponse,
  GrantEntry,
  AuditResponse,
  PeersResponse,
  PinsResponse,
  PinEntry,
  PoliciesResponse,
  PolicyEntry,
  BackendsResponse,
  CacheActionResponse,
  CapabilityToken,
} from './types';

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function isStringField(value: Record<string, unknown>, key: string): boolean {
  return key in value && typeof value[key] === 'string';
}

export function isCapabilityToken(value: unknown): value is CapabilityToken {
  if (!isRecord(value)) return false;
  return isStringField(value, 'token_id')
    && isStringField(value, 'issuer')
    && isStringField(value, 'bearer')
    && isStringField(value, 'capability')
    && isStringField(value, 'expires')
    && isStringField(value, 'issued_at')
    && 'scope' in value;
}

export function isSessionResponse(value: unknown): value is SessionResponse {
  if (!isRecord(value)) return false;
  return 'session' in value && 'token' in value && 'abilities' in value;
}

export function isHealthResponse(value: unknown): value is HealthResponse {
  if (!isRecord(value)) return false;
  return value.status === 'ok';
}

export function isReadyResponse(value: unknown): value is ReadyResponse {
  if (!isRecord(value)) return false;
  return 'ready' in value && 'started_at' in value;
}

export function isFolderChildrenResponse(value: unknown): value is FolderChildrenResponse {
  if (!isRecord(value)) return false;
  return 'folder' in value && 'entries' in value && Array.isArray(value.entries);
}

export function isEntryMetaResponse(value: unknown): value is EntryMetaResponse {
  if (!isRecord(value)) return false;
  return 'name' in value && 'kind' in value;
}

export function isSharesResponse(value: unknown): value is SharesResponse {
  if (!isRecord(value)) return false;
  return 'shares' in value && Array.isArray(value.shares);
}

export function isShareEntry(value: unknown): value is ShareEntry {
  if (!isRecord(value)) return false;
  return 'peer_device_id' in value && 'folder' in value && 'posture' in value;
}

export function isTokensResponse(value: unknown): value is TokensResponse {
  if (!isRecord(value)) return false;
  return 'tokens' in value && Array.isArray(value.tokens);
}

export function isRevokeTokenResponse(value: unknown): value is RevokeTokenResponse {
  if (!isRecord(value)) return false;
  return 'revoked_at' in value;
}

export function isGrantsResponse(value: unknown): value is GrantsResponse {
  if (!isRecord(value)) return false;
  return 'grants' in value && Array.isArray(value.grants);
}

export function isGrantEntry(value: unknown): value is GrantEntry {
  if (!isRecord(value)) return false;
  return 'grantee' in value && 'capability' in value;
}

export function isAuditResponse(value: unknown): value is AuditResponse {
  if (!isRecord(value)) return false;
  return 'entries' in value && Array.isArray(value.entries);
}

export function isPeersResponse(value: unknown): value is PeersResponse {
  if (!isRecord(value)) return false;
  return 'peers' in value && Array.isArray(value.peers);
}

export function isPinsResponse(value: unknown): value is PinsResponse {
  if (!isRecord(value)) return false;
  return 'pins' in value && Array.isArray(value.pins);
}

export function isPinEntry(value: unknown): value is PinEntry {
  if (!isRecord(value)) return false;
  return 'path_glob' in value && 'folder' in value;
}

export function isPoliciesResponse(value: unknown): value is PoliciesResponse {
  if (!isRecord(value)) return false;
  return 'policies' in value && Array.isArray(value.policies);
}

export function isPolicyEntry(value: unknown): value is PolicyEntry {
  if (!isRecord(value)) return false;
  return 'folder' in value && 'rule' in value;
}

export function isBackendsResponse(value: unknown): value is BackendsResponse {
  if (!isRecord(value)) return false;
  return 'backends' in value && Array.isArray(value.backends);
}

export function isCacheActionResponse(value: unknown): value is CacheActionResponse {
  if (!isRecord(value)) return false;
  return 'summary' in value;
}

export function isDeviceCodeResponse(value: unknown): value is { code: string; expires_in: number } {
  if (!isRecord(value)) return false;
  return isStringField(value, 'code') && typeof value.expires_in === 'number';
}

export function isDevicePollResponse(value: unknown): value is { status: string; token?: CapabilityToken } {
  if (!isRecord(value)) return false;
  return isStringField(value, 'status');
}
