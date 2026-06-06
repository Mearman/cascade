// Google Drive OAuth redirect flow with PKCE for the WASM engine.
// Desktop OAuth clients do not require a client secret in the browser;
// PKCE (RFC 7636) provides the equivalent proof of possession.

const GOOGLE_AUTH_ENDPOINT = 'https://accounts.google.com/o/oauth2/v2/auth';
const GOOGLE_TOKEN_ENDPOINT = 'https://oauth2.googleapis.com/token';

const STATE_SESSION_KEY = 'cascade-oauth-state';
const VERIFIER_SESSION_KEY = 'cascade-oauth-verifier';

const AUTH_DB_NAME = 'cascade-auth';
const AUTH_DB_VERSION = 1;
const AUTH_STORE_NAME = 'tokens';

export interface OAuthTokens {
  access_token: string;
  refresh_token: string;
  expiry: number; // unix timestamp ms
  scope: string;
}

export interface OAuthConfig {
  clientId: string;
  clientSecret?: string;
  redirectUri: string;
  scopes: string[];
}

// Scopes required to match the CLI's device code flow.
export const GDRIVE_SCOPES = [
  'https://www.googleapis.com/auth/drive',
  'https://www.googleapis.com/auth/drive.file',
] as const;

// Stored alongside the tokens so refresh can proceed without re-initiating auth.
let cachedClientId: string | null = null;

// ─── PKCE helpers ─────────────────────────────────────────────────────────────

function base64UrlEncode(bytes: Uint8Array): string {
  const chars = Array.from(bytes, (b) => String.fromCharCode(b));
  return btoa(chars.join(''))
    .replace(/\+/g, '-')
    .replace(/\//g, '_')
    .replace(/=/g, '');
}

function generateCodeVerifier(): string {
  return base64UrlEncode(crypto.getRandomValues(new Uint8Array(32)));
}

async function generateCodeChallenge(verifier: string): Promise<string> {
  const encoded = new TextEncoder().encode(verifier);
  const digest = await crypto.subtle.digest('SHA-256', encoded);
  return base64UrlEncode(new Uint8Array(digest));
}

// ─── IndexedDB helpers ────────────────────────────────────────────────────────

function openAuthDb(): Promise<IDBDatabase> {
  return new Promise((resolve, reject) => {
    const req = indexedDB.open(AUTH_DB_NAME, AUTH_DB_VERSION);
    req.onupgradeneeded = () => {
      const db = req.result;
      if (!db.objectStoreNames.contains(AUTH_STORE_NAME)) {
        db.createObjectStore(AUTH_STORE_NAME, { keyPath: 'provider' });
      }
    };
    req.onsuccess = () => { resolve(req.result); };
    req.onerror = () => { reject(new Error(String(req.error))); };
  });
}

interface StoredTokenRecord {
  provider: string;
  access_token: string;
  refresh_token: string;
  expiry: number;
  scope: string;
  client_id: string;
}

function isStoredTokenRecord(value: unknown): value is StoredTokenRecord {
  if (typeof value !== 'object' || value === null) return false;
  if (!('provider' in value) || typeof value.provider !== 'string') return false;
  if (!('access_token' in value) || typeof value.access_token !== 'string') return false;
  if (!('refresh_token' in value) || typeof value.refresh_token !== 'string') return false;
  if (!('expiry' in value) || typeof value.expiry !== 'number') return false;
  if (!('scope' in value) || typeof value.scope !== 'string') return false;
  if (!('client_id' in value) || typeof value.client_id !== 'string') return false;
  return true;
}

interface GoogleTokenResponse {
  access_token: string;
  expires_in: number;
  refresh_token?: string;
  scope: string;
  token_type: string;
}

interface GoogleErrorResponse {
  error: string;
  error_description?: string;
}

function isGoogleTokenResponse(value: unknown): value is GoogleTokenResponse {
  if (typeof value !== 'object' || value === null) return false;
  if (!('access_token' in value) || typeof value.access_token !== 'string') return false;
  if (!('expires_in' in value) || typeof value.expires_in !== 'number') return false;
  if (!('scope' in value) || typeof value.scope !== 'string') return false;
  return true;
}

function isGoogleErrorResponse(value: unknown): value is GoogleErrorResponse {
  if (typeof value !== 'object' || value === null) return false;
  return 'error' in value && typeof value.error === 'string';
}

// ─── Public API ───────────────────────────────────────────────────────────────

// Redirect the browser to Google's authorisation endpoint.
// Generates and stores the PKCE verifier and CSRF state before redirecting.
export async function initiateAuth(config: OAuthConfig): Promise<void> {
  cachedClientId = config.clientId;

  const verifier = generateCodeVerifier();
  const challenge = await generateCodeChallenge(verifier);
  const state = base64UrlEncode(crypto.getRandomValues(new Uint8Array(16)));

  sessionStorage.setItem(STATE_SESSION_KEY, state);
  sessionStorage.setItem(VERIFIER_SESSION_KEY, verifier);

  const params = new URLSearchParams({
    client_id: config.clientId,
    redirect_uri: config.redirectUri,
    response_type: 'code',
    scope: config.scopes.join(' '),
    state,
    code_challenge: challenge,
    code_challenge_method: 'S256',
    access_type: 'offline',
    prompt: 'consent', // always return a refresh_token
  });

  window.location.href = `${GOOGLE_AUTH_ENDPOINT}?${params.toString()}`;
}

// Called on the redirect-callback page. Validates state, exchanges the
// authorisation code for tokens, and persists them to IndexedDB.
export async function handleCallback(): Promise<OAuthTokens> {
  const params = new URLSearchParams(window.location.search);

  const error = params.get('error');
  if (error !== null) {
    const desc = params.get('error_description') ?? error;
    throw new Error(`OAuth error: ${desc}`);
  }

  const code = params.get('code');
  if (code === null) throw new Error('OAuth callback: missing code parameter');

  const returnedState = params.get('state');
  const storedState = sessionStorage.getItem(STATE_SESSION_KEY);
  if (returnedState === null || returnedState !== storedState) {
    throw new Error('OAuth callback: state mismatch — possible CSRF attack');
  }

  const verifier = sessionStorage.getItem(VERIFIER_SESSION_KEY);
  if (verifier === null) throw new Error('OAuth callback: missing code verifier');

  sessionStorage.removeItem(STATE_SESSION_KEY);
  sessionStorage.removeItem(VERIFIER_SESSION_KEY);

  // Recover clientId from the cached value set during initiateAuth,
  // or from the stored token record if a previous session exists.
  let clientId = cachedClientId;
  if (clientId === null) {
    const stored = await getStoredTokens();
    if (stored === null) {
      throw new Error('OAuth callback: client ID unavailable — call initiateAuth first');
    }
    // clientId was saved in IndexedDB alongside the token record
    const db = await openAuthDb();
    const record: unknown = await new Promise((resolve, reject) => {
      const tx = db.transaction(AUTH_STORE_NAME, 'readonly');
      const req = tx.objectStore(AUTH_STORE_NAME).get('gdrive');
      req.onsuccess = () => { resolve(req.result); };
      req.onerror = () => { reject(new Error(String(req.error))); };
    });
    if (!isStoredTokenRecord(record)) {
      throw new Error('OAuth callback: stored token record corrupt');
    }
    clientId = record.client_id;
  }

  const body = new URLSearchParams({
    code,
    client_id: clientId,
    redirect_uri: new URL(window.location.href).origin + window.location.pathname,
    code_verifier: verifier,
    grant_type: 'authorization_code',
  });

  const resp = await fetch(GOOGLE_TOKEN_ENDPOINT, {
    method: 'POST',
    headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
    body: body.toString(),
  });

  const json: unknown = await resp.json();

  if (!resp.ok) {
    if (isGoogleErrorResponse(json)) {
      throw new Error(`Token exchange failed: ${json.error_description ?? json.error}`);
    }
    throw new Error(`Token exchange failed with status ${String(resp.status)}`);
  }

  if (!isGoogleTokenResponse(json)) {
    throw new Error('Token exchange: unexpected response shape');
  }

  if (json.refresh_token === undefined) {
    throw new Error('Token exchange: no refresh_token returned (add prompt=consent to the auth request)');
  }

  const tokens: OAuthTokens = {
    access_token: json.access_token,
    refresh_token: json.refresh_token,
    expiry: Date.now() + json.expires_in * 1000,
    scope: json.scope,
  };

  await storeTokens(tokens);
  return tokens;
}

// Exchange a refresh token for a new access token.
export async function refreshTokens(refreshToken: string): Promise<OAuthTokens> {
  // Resolve clientId from cache or IndexedDB.
  let clientId = cachedClientId;
  if (clientId === null) {
    const db = await openAuthDb();
    const record: unknown = await new Promise((resolve, reject) => {
      const tx = db.transaction(AUTH_STORE_NAME, 'readonly');
      const req = tx.objectStore(AUTH_STORE_NAME).get('gdrive');
      req.onsuccess = () => { resolve(req.result); };
      req.onerror = () => { reject(new Error(String(req.error))); };
    });
    if (!isStoredTokenRecord(record)) {
      throw new Error('refreshTokens: no stored token record found — call initiateAuth first');
    }
    clientId = record.client_id;
  }

  const body = new URLSearchParams({
    client_id: clientId,
    refresh_token: refreshToken,
    grant_type: 'refresh_token',
  });

  const resp = await fetch(GOOGLE_TOKEN_ENDPOINT, {
    method: 'POST',
    headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
    body: body.toString(),
  });

  const json: unknown = await resp.json();

  if (!resp.ok) {
    if (isGoogleErrorResponse(json)) {
      throw new Error(`Token refresh failed: ${json.error_description ?? json.error}`);
    }
    throw new Error(`Token refresh failed with status ${String(resp.status)}`);
  }

  if (!isGoogleTokenResponse(json)) {
    throw new Error('Token refresh: unexpected response shape');
  }

  // Google does not always return a new refresh_token on refresh; keep the original.
  const tokens: OAuthTokens = {
    access_token: json.access_token,
    refresh_token: json.refresh_token ?? refreshToken,
    expiry: Date.now() + json.expires_in * 1000,
    scope: json.scope,
  };

  await storeTokens(tokens);
  return tokens;
}

// Retrieve persisted tokens from IndexedDB. Returns null if none are stored.
export async function getStoredTokens(): Promise<OAuthTokens | null> {
  const db = await openAuthDb();
  const record: unknown = await new Promise((resolve, reject) => {
    const tx = db.transaction(AUTH_STORE_NAME, 'readonly');
    const req = tx.objectStore(AUTH_STORE_NAME).get('gdrive');
    req.onsuccess = () => { resolve(req.result); };
    req.onerror = () => { reject(new Error(String(req.error))); };
  });
  if (!isStoredTokenRecord(record)) return null;
  cachedClientId = record.client_id;
  return {
    access_token: record.access_token,
    refresh_token: record.refresh_token,
    expiry: record.expiry,
    scope: record.scope,
  };
}

// Persist tokens to IndexedDB. Requires cachedClientId to have been set by
// initiateAuth or a prior getStoredTokens call.
export async function storeTokens(tokens: OAuthTokens): Promise<void> {
  if (cachedClientId === null) {
    throw new Error('storeTokens: no clientId available — call initiateAuth or getStoredTokens first');
  }
  const record: StoredTokenRecord = {
    provider: 'gdrive',
    client_id: cachedClientId,
    ...tokens,
  };
  const db = await openAuthDb();
  await new Promise<void>((resolve, reject) => {
    const tx = db.transaction(AUTH_STORE_NAME, 'readwrite');
    const req = tx.objectStore(AUTH_STORE_NAME).put(record);
    req.onsuccess = () => { resolve(); };
    req.onerror = () => { reject(new Error(String(req.error))); };
  });
}
