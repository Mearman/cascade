// Google Drive REST API client for the WASM engine.
//
// Fetches file listings from the Drive API using the access token stored in
// IndexedDB by `oauth.ts`, converts them to engine-compatible file entries,
// and upserts them into engine storage via the `upsertFiles` mutator.

import type { FileInput } from '@/wasm/bridge';
import { getStoredTokens, refreshTokens } from '@/wasm/oauth';
import { api } from '@/api/client';

const DRIVE_FILES_ENDPOINT = 'https://www.googleapis.com/drive/v3/files';

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
