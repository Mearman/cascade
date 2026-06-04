import { api, API_BASE_KEY, TOKEN_KEY } from '@/api/client';
import { ApiError } from '@/api/types';
import type { CapabilityToken, SessionResponse } from '@/api/types';

export { API_BASE_KEY };

export function isCapabilityToken(value: unknown): value is CapabilityToken {
  if (typeof value !== 'object' || value === null) return false;
  if (!('token_id' in value) || typeof value.token_id !== 'string') return false;
  if (!('issuer' in value) || typeof value.issuer !== 'string') return false;
  if (!('bearer' in value) || typeof value.bearer !== 'string') return false;
  if (!('capability' in value) || typeof value.capability !== 'string') return false;
  if (!('scope' in value) || typeof value.scope !== 'object' || value.scope === null) return false;
  if (!('expires' in value) || typeof value.expires !== 'string') return false;
  if (!('issued_at' in value) || typeof value.issued_at !== 'string') return false;
  return true;
}

export function saveToken(token: CapabilityToken): void {
  try {
    localStorage.setItem(TOKEN_KEY, JSON.stringify(token));
    api.setToken(token);
  } catch (err) {
    console.error('Auth: failed to save token', err);
  }
}

export function loadToken(): CapabilityToken | null {
  try {
    const raw = localStorage.getItem(TOKEN_KEY);
    if (raw === null) return null;
    const parsed: unknown = JSON.parse(raw);
    if (!isCapabilityToken(parsed)) return null;
    api.setToken(parsed);
    return parsed;
  } catch {
    return null;
  }
}

export function clearToken(): void {
  try {
    localStorage.removeItem(TOKEN_KEY);
    api.setToken(null);
  } catch {
    // ignore
  }
}

export function getToken(): CapabilityToken | null {
  return api.getToken();
}

export function getApiBase(): string {
  return localStorage.getItem(API_BASE_KEY) ?? '';
}

export function saveApiBase(base: string): void {
  localStorage.setItem(API_BASE_KEY, base);
}

export function clearApiBase(): void {
  localStorage.removeItem(API_BASE_KEY);
}

export function hasApiBase(): boolean {
  return localStorage.getItem(API_BASE_KEY) !== null;
}

// Validate token against the daemon by calling /v1/session.
// Returns the session response on success, throws ApiError on failure.
export async function validateToken(token: CapabilityToken): Promise<SessionResponse> {
  saveToken(token);
  try {
    return await api.session();
  } catch (err) {
    clearToken();
    throw err;
  }
}

export function initAuth(on401: () => void): void {
  loadToken();
  api.setOn401(on401);
}
