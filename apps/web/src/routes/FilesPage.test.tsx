import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { render, waitFor } from '@testing-library/preact';
import { AppContext } from '@/context';
import { RuntimeMode } from '@/wasm';
import type { AppContextValue } from '@/context';
import type { BackendEntry } from '@/api/types';

// Mock the api client module before importing the route. The route imports
// `api` as a singleton; replacing the whole module lets us swap the methods
// used during the initial mount effect. `vi.hoisted` runs the mock-object
// definition in the same hoisted scope as `vi.mock`, so the factory can close
// over it without tripping the temporal-dead-zone error.
const apiMock = vi.hoisted(() => ({
  rawRequest: vi.fn(),
  backends: vi.fn(),
  folderChildren: vi.fn(),
}));

vi.mock('@/api/client', () => ({ api: apiMock }));

// The route also imports from the gdrive helpers. Stub them so a Drive fetch
// is never attempted in tests.
vi.mock('@/wasm/gdrive', () => ({
  fetchFolderChildren: vi.fn(() => Promise.resolve(0)),
  downloadDriveFile: vi.fn(),
  uploadDriveFile: vi.fn(),
  deleteDriveFile: vi.fn(),
}));

import { FilesPage } from './FilesPage';

function makeContext(mode: RuntimeMode): AppContextValue {
  return {
    mode,
    capabilities: {
      fileSystemAccess: false,
      webRtc: false,
      serviceWorker: false,
      wasm: mode !== RuntimeMode.Connected,
      indexedDb: false,
    },
    directoryName: null,
    setDirectoryName: () => {
      // No-op: directory selection is not exercised by these tests.
    },
  };
}

function renderWithMode(mode: RuntimeMode) {
  return render(
    <AppContext.Provider value={makeContext(mode)}>
      <FilesPage />
    </AppContext.Provider>,
  );
}

beforeEach(() => {
  apiMock.rawRequest.mockReset();
  apiMock.backends.mockReset();
  apiMock.folderChildren.mockReset();
});

afterEach(() => {
  vi.clearAllMocks();
});

describe('FilesPage initial state', () => {
  it('renders a Spinner before any backends have loaded', () => {
    apiMock.rawRequest.mockReturnValue(new Promise(() => {
      // Never settles: holds the component in its loading state.
    }));
    const { container } = renderWithMode(RuntimeMode.Standalone);
    const spinner = container.querySelector('[role="status"]');
    expect(spinner).not.toBeNull();
  });
});

describe('FilesPage in WASM mode', () => {
  it('lists the configured backends once the engine responds', async () => {
    // Route by path: the backend list for GET /v1/backends, an empty child
    // list for the follow-up folder-children fetch so the row render loop
    // doesn't try to hit Drive.
    apiMock.rawRequest.mockImplementation((_method: string, path: string) => {
      if (path === '/v1/backends') {
        return Promise.resolve({
          status: 200,
          body: {
            backends: [
              { id: 'gdrive-personal', type: 'gdrive', display_name: 'Personal', hasHandle: true },
              { id: 'gdrive-work', type: 'gdrive', display_name: 'Work', hasHandle: false },
            ],
          },
        });
      }
      return Promise.resolve({ status: 200, body: { children: [] } });
    });

    const { getByText, findAllByText } = renderWithMode(RuntimeMode.Standalone);

    // "Personal" is the selected backend, so it appears both in the backend
    // picker option and the breadcrumb; "Work" appears only in the option.
    await findAllByText('Personal');
    expect(getByText('Work')).toBeTruthy();
    // Verify the initial call was the backend list.
    expect(apiMock.rawRequest).toHaveBeenCalledWith('GET', '/v1/backends');
  });

  it('shows the empty state with a login link when no backends are configured', async () => {
    apiMock.rawRequest.mockResolvedValue({
      status: 200,
      body: { backends: [] },
    });

    const { findByText } = renderWithMode(RuntimeMode.BrowseOnly);

    const message = await findByText(/No backends registered/);
    expect(message.textContent).toMatch(/Log in/);
  });

  it('surfaces an error from the engine', async () => {
    apiMock.rawRequest.mockResolvedValue({
      status: 500,
      body: null,
    });

    const { findByText } = renderWithMode(RuntimeMode.Standalone);

    const banner = await findByText(/Failed to load backends/);
    expect(banner).toBeTruthy();
  });

  it('shows an empty-directory message when the engine returns no children', async () => {
    // First call: backend list with one entry.
    apiMock.rawRequest.mockResolvedValueOnce({
      status: 200,
      body: { backends: [{ id: 'gdrive-1', type: 'gdrive', display_name: 'Drive', hasHandle: true }] },
    });
    // Second call: folder children (empty list).
    apiMock.rawRequest.mockResolvedValueOnce({
      status: 200,
      body: { children: [] },
    });

    const { findByText } = renderWithMode(RuntimeMode.Standalone);

    const empty = await findByText('Empty directory.');
    expect(empty).toBeTruthy();
  });
});

describe('FilesPage in Connected mode', () => {
  it('lists P2P backends from the daemon', async () => {
    const p2pBackends: BackendEntry[] = [
      {
        id: 'b1',
        name: 'Shared',
        folder_id: 'folder-1',
        mount_path: '/Shared',
        healthy: true,
      },
      {
        id: 'b2',
        name: 'No folder',
        folder_id: null,
        mount_path: '/NoFolder',
        healthy: true,
      },
    ];
    apiMock.backends.mockResolvedValue({ backends: p2pBackends });
    apiMock.folderChildren.mockResolvedValue({
      folder: 'folder-1',
      path: '',
      entries: [],
      next_cursor: null,
    });

    const { findByText, queryByText } = renderWithMode(RuntimeMode.Connected);

    await findByText('Shared (folder-1)');
    // The null-folder backend is filtered out by the P2P predicate.
    expect(queryByText('No folder')).toBeNull();
  });

  it('shows the empty state with a P2P-specific message when no P2P backends exist', async () => {
    apiMock.backends.mockResolvedValue({ backends: [] });
    const { findByText } = renderWithMode(RuntimeMode.Connected);
    const message = await findByText('No P2P backends configured.');
    expect(message).toBeTruthy();
  });

  it('renders the loading spinner synchronously', () => {
    apiMock.backends.mockReturnValue(new Promise(() => {
      // Never settles: holds the component in its loading state.
    }));
    const { container } = renderWithMode(RuntimeMode.Connected);
    expect(container.querySelector('[role="status"]')).not.toBeNull();
  });
});

// Sanity: when initial mount triggers backends() that resolves successfully,
// the page eventually settles into a non-spinner state.
describe('FilesPage renders without error after load', () => {
  it('replaces the spinner once backends resolve', async () => {
    apiMock.rawRequest.mockResolvedValue({
      status: 200,
      body: { backends: [] },
    });

    const { container } = renderWithMode(RuntimeMode.Standalone);

    await waitFor(() => {
      expect(container.querySelector('[role="status"]')).toBeNull();
    });
  });
});
