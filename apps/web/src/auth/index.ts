/**
 * Auth store — token entry, localStorage persistence, and 401 interceptor.
 *
 * Tokens are stored in localStorage, which is only accessible from a Secure
 * Context (HTTPS or localhost). If the page is served over an insecure
 * connection the store refuses to persist or retrieve tokens.
 */

import { api } from '@/api';
import type { AuthToken } from '@/api/types';

const STORAGE_KEY = 'cascade-auth-token';

function isSecureContext(): boolean {
  return window.location.protocol === 'https:' || window.location.hostname === 'localhost' || window.location.hostname === '127.0.0.1';
}

export function saveToken(token: AuthToken): void {
  if (!isSecureContext()) {
    console.warn('Auth: not a secure context, refusing to store token in localStorage');
    return;
  }
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(token));
    api.setToken(token);
  } catch (err) {
    console.error('Auth: failed to save token', err);
  }
}

export function loadToken(): AuthToken | null {
  if (!isSecureContext()) {
    return null;
  }
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (raw === null) return null;
    const token = JSON.parse(raw) as AuthToken;
    api.setToken(token);
    return token;
  } catch {
    return null;
  }
}

export function clearToken(): void {
  if (!isSecureContext()) return;
  try {
    localStorage.removeItem(STORAGE_KEY);
    api.setToken(null);
  } catch {
    // ignore
  }
}

export function getToken(): AuthToken | null {
  return api.getToken();
}

// Interceptor: attach the stored token to every fetch request and redirect
// to the login page on 401.
export function init401Interceptor(on401: () => void): () => void {
  const originalFetch = window.fetch.bind(window);
  window.fetch = async function fetch(input: RequestInfo | URL, init?: RequestInit) {
    const token = loadToken();
    if (token) {
      api.setToken(token);
    }
    try {
      const response = await originalFetch(input, init);
      if (response.status === 401) {
        clearToken();
        on401();
      }
      return response;
    } catch (err) {
      return originalFetch(input, init);
    }
  };

  return () => {
    window.fetch = originalFetch;
  };
}
