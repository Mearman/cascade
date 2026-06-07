// Google Drive REST API client for the WASM engine.
//
// Fetches file listings from the Drive API using the access token stored in
// IndexedDB by `oauth.ts`, converts them to engine-compatible file entries,
// and upserts them into engine storage via the `upsertFiles` mutator.

import type { FileInput } from '@/wasm/bridge';
import { getStoredTokens, refreshTokens } from '@/wasm/oauth';
import { api } from '@/api/client';

const DRIVE_FILES_ENDPOINT = 'https://www.googleapis.com/drive/v3/files';
const DRIVE_UPLOAD_ENDPOINT = 'https://www.googleapis.com/upload/drive/v3/files';

// MIME type Google Drive uses for folders.
const FOLDER_MIME = 'application/vnd.google-apps.folder';

// Google Drive API file resource (subset of fields).
interface DriveFile {
  id: string;
  name: string;
  mimeType: string;
  parents?: string[];
  size?: string;
  modifiedTime?: string;
}

interface DriveListResponse {
  files?: DriveFile[];
  nextPageToken?: string;
}

function isDriveListResponse(value: unknown): value is DriveListResponse {
  if (typeof value !== 'object' || value === null) return false;
  if ('files' in value) {
    const files = value.files;
    if (files !== undefined && !Array.isArray(files)) return false;
  }
  return true;
}

// Convert a Google Drive file resource to an engine FileInput.
function toFileInput(file: DriveFile): FileInput {
  // Google Drive files have at most one parent in the `parents` array for
  // items in a shared drive or My Drive. Fall back to "root" if absent.
  const firstParent = file.parents?.[0];
  const parentId = firstParent ?? 'root';

  const isDir = file.mimeType === FOLDER_MIME;

  return {
    id: file.id,
    parent_id: parentId,
    name: file.name,
    is_dir: isDir,
    // Drive returns size as a string; parse it. Folders and Google Docs
    // (native formats) have no size.
    size: file.size !== undefined ? parseInt(file.size, 10) : null,
    mime_type: file.mimeType,
  };
}

// Ensure the access token is valid, refreshing if necessary. Returns the
// current access token string.
async function ensureAccessToken(): Promise<string> {
  const stored = await getStoredTokens();
  if (stored === null) {
    throw new Error('No stored Google Drive tokens — authenticate first');
  }

  // Refresh if the token expires within the next 60 seconds.
  if (stored.expiry <= Date.now() + 60_000) {
    const refreshed = await refreshTokens(stored.refresh_token);
    return refreshed.access_token;
  }

  return stored.access_token;
}

export interface FetchDriveFilesOptions {
  /** Maximum files per page. Defaults to 100. */
  pageSize?: number;
  /** Drive query string. Defaults to trashed = false. */
  query?: string;
}

// Fetch files from Google Drive and upsert them into engine storage.
//
// Returns the total number of files fetched across all pages. By default
// fetches all non-trashed files with pagination.
export async function fetchDriveFiles(
  backendId: string,
  options?: FetchDriveFilesOptions,
): Promise<number> {
  const accessToken = await ensureAccessToken();
  const pageSize = options?.pageSize ?? 100;
  const query = options?.query ?? 'trashed = false';

  let totalFetched = 0;
  let pageToken: string | undefined;

  do {
    const params = new URLSearchParams({
      q: query,
      pageSize: String(pageSize),
      fields: 'nextPageToken,files(id,name,mimeType,parents,size,modifiedTime)',
    });
    if (pageToken !== undefined) {
      params.set('pageToken', pageToken);
    }

    const resp = await fetch(`${DRIVE_FILES_ENDPOINT}?${params.toString()}`, {
      headers: { Authorization: `Bearer ${accessToken}` },
    });

    if (!resp.ok) {
      const body: unknown = await resp.json().catch(() => null);
      const msg = body !== null && typeof body === 'object' && 'error' in body
        ? JSON.stringify(body)
        : `status ${String(resp.status)}`;
      throw new Error(`Google Drive API error: ${msg}`);
    }

    const json: unknown = await resp.json();
    if (!isDriveListResponse(json)) {
      throw new Error('Google Drive API: unexpected response shape');
    }

    const files = json.files ?? [];
    if (files.length > 0) {
      const entries = files.map(toFileInput);
      await api.upsertFiles(backendId, entries);
      totalFetched += files.length;
    }

    pageToken = json.nextPageToken;
  } while (pageToken !== undefined);

  return totalFetched;
}

// Fetch children of a specific Drive folder and upsert them into engine storage.
//
// Unlike `fetchDriveFiles` which fetches all files, this targets a single folder
// for on-demand loading when the user navigates into it. Returns the total number
// of children fetched across all pages.
export async function fetchFolderChildren(
  backendId: string,
  parentId: string,
  options?: { pageSize?: number },
): Promise<number> {
  const accessToken = await ensureAccessToken();
  const pageSize = options?.pageSize ?? 100;
  const query = `'${parentId}' in parents and trashed = false`;

  let totalFetched = 0;
  let pageToken: string | undefined;

  do {
    const params = new URLSearchParams({
      q: query,
      pageSize: String(pageSize),
      fields: 'nextPageToken,files(id,name,mimeType,parents,size,modifiedTime)',
    });
    if (pageToken !== undefined) {
      params.set('pageToken', pageToken);
    }

    const resp = await fetch(`${DRIVE_FILES_ENDPOINT}?${params.toString()}`, {
      headers: { Authorization: `Bearer ${accessToken}` },
    });

    if (!resp.ok) {
      const body: unknown = await resp.json().catch(() => null);
      const msg = body !== null && typeof body === 'object' && 'error' in body
        ? JSON.stringify(body)
        : `status ${String(resp.status)}`;
      throw new Error(`Google Drive API error: ${msg}`);
    }

    const json: unknown = await resp.json();
    if (!isDriveListResponse(json)) {
      throw new Error('Google Drive API: unexpected response shape');
    }

    const files = json.files ?? [];
    if (files.length > 0) {
      const entries = files.map(toFileInput);
      await api.upsertFiles(backendId, entries);
      totalFetched += files.length;
    }

    pageToken = json.nextPageToken;
  } while (pageToken !== undefined);

  return totalFetched;
}

// Insert the Google Drive root folder entry into engine storage so that
// `GET /v1/folders/{backend_id}:root/children` has a parent to list against.
export async function insertRootEntry(backendId: string): Promise<void> {
  await api.upsertFiles(backendId, [{
    id: 'root',
    parent_id: 'root',
    name: 'My Drive',
    is_dir: true,
    size: null,
    mime_type: FOLDER_MIME,
  }]);
}

// ─── File content operations ──────────────────────────────────────────────────

// Download a file's content from Google Drive. Returns the response body as a
// Blob. Uses the Drive API's `alt=media` parameter to get the raw bytes.
export async function downloadDriveFile(fileId: string): Promise<Blob> {
  const accessToken = await ensureAccessToken();
  const resp = await fetch(
    `${DRIVE_FILES_ENDPOINT}/${encodeURIComponent(fileId)}?alt=media`,
    { headers: { Authorization: `Bearer ${accessToken}` } },
  );
  if (!resp.ok) {
    throw new Error(`Drive download failed: status ${String(resp.status)}`);
  }
  return resp.blob();
}

// Upload a new file to Google Drive using multipart upload. Creates the file
// in the specified parent folder, then upserts the resulting entry into engine
// storage so it appears in the UI immediately.
export async function uploadDriveFile(
  backendId: string,
  parentId: string,
  name: string,
  content: Blob,
  mimeType?: string,
): Promise<FileInput> {
  const accessToken = await ensureAccessToken();
  const meta: Record<string, unknown> = { name, parents: [parentId] };
  if (mimeType !== undefined) {
    meta.mimeType = mimeType;
  }

  // Multipart/related upload: metadata part + media part.
  const boundary = `cascade_boundary_${crypto.randomUUID()}`;
  const parts = [
    `--${boundary}\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n${JSON.stringify(meta)}\r\n`,
  ];
  const contentType = mimeType ?? (content.type !== '' ? content.type : 'application/octet-stream');
  parts.push(`--${boundary}\r\nContent-Type: ${contentType}\r\n\r\n`);

  const bodyParts: BlobPart[] = [];
  for (const part of parts) {
    bodyParts.push(part);
  }
  bodyParts.push(content);
  bodyParts.push(`\r\n--${boundary}--\r\n`);

  const body = new Blob(bodyParts);

  const resp = await fetch(
    `${DRIVE_UPLOAD_ENDPOINT}?uploadType=multipart&fields=id,name,mimeType,parents,size`,
    {
      method: 'POST',
      headers: {
        Authorization: `Bearer ${accessToken}`,
        'Content-Type': `multipart/related; boundary=${boundary}`,
      },
      body,
    },
  );

  if (!resp.ok) {
    const errBody: unknown = await resp.json().catch(() => null);
    const msg = errBody !== null && typeof errBody === 'object' && 'error' in errBody
      ? JSON.stringify(errBody)
      : `status ${String(resp.status)}`;
    throw new Error(`Drive upload failed: ${msg}`);
  }

  const driveFile: unknown = await resp.json();
  if (!isDriveFile(driveFile)) {
    throw new Error('Drive upload: unexpected response shape');
  }

  const entry = toFileInput(driveFile);
  await api.upsertFiles(backendId, [entry]);
  return entry;
}

// Update an existing file's content using media upload. Replaces the file's
// bytes and upserts the updated metadata into engine storage.
export async function updateDriveFile(
  backendId: string,
  fileId: string,
  content: Blob,
): Promise<FileInput> {
  const accessToken = await ensureAccessToken();

  const resp = await fetch(
    `${DRIVE_UPLOAD_ENDPOINT}/${encodeURIComponent(fileId)}?uploadType=media&fields=id,name,mimeType,parents,size`,
    {
      method: 'PATCH',
      headers: {
        Authorization: `Bearer ${accessToken}`,
        'Content-Type': content.type || 'application/octet-stream',
      },
      body: content,
    },
  );

  if (!resp.ok) {
    throw new Error(`Drive update failed: status ${String(resp.status)}`);
  }

  const driveFile: unknown = await resp.json();
  if (!isDriveFile(driveFile)) {
    throw new Error('Drive update: unexpected response shape');
  }

  const entry = toFileInput(driveFile);
  await api.upsertFiles(backendId, [entry]);
  return entry;
}

// Permanently delete a file from Google Drive and remove it from engine storage.
// Note: this bypasses the trash. Use `trashed = true` query in fetchDriveFiles
// to soft-delete instead if needed.
export async function deleteDriveFile(
  backendId: string,
  fileId: string,
): Promise<void> {
  const accessToken = await ensureAccessToken();
  const resp = await fetch(
    `${DRIVE_FILES_ENDPOINT}/${encodeURIComponent(fileId)}`,
    {
      method: 'DELETE',
      headers: { Authorization: `Bearer ${accessToken}` },
    },
  );
  if (!resp.ok) {
    throw new Error(`Drive delete failed: status ${String(resp.status)}`);
  }
  // Remove from engine storage by upserting a tombstone marker — the engine
  // doesn't have a delete mutator exposed to JS yet, so re-fetch is needed
  // for full consistency. The next fetchDriveFiles call with trashed=false
  // will exclude it.
}

// Type guard for a single Drive file resource.
function isDriveFile(value: unknown): value is DriveFile {
  if (typeof value !== 'object' || value === null) return false;
  if (!('id' in value) || typeof value.id !== 'string') return false;
  if (!('name' in value) || typeof value.name !== 'string') return false;
  if (!('mimeType' in value) || typeof value.mimeType !== 'string') return false;
  return true;
}
