import { Route, Router, Switch } from 'wouter-preact';
import { AppShell } from '@/components';
import {
  DashboardPage,
  FilesPage,
  GrantsPage,
  LoginPage,
  SharesPage,
  TokensPage,
} from '@/routes';
import { loadToken } from '@/auth';
import type { AuthToken, Role } from '@/api/types';

// Lazy-load heavier routes.
const LazyDashboard = () => import('@/routes/DashboardPage').then((m) => ({ default: m.DashboardPage }));
const LazyFiles = () => import('@/routes/FilesPage').then((m) => ({ default: m.FilesPage }));
const LazyGrants = () => import('@/routes/GrantsPage').then((m) => ({ default: m.GrantsPage }));
const LazyShares = () => import('@/routes/SharesPage').then((m) => ({ default: m.SharesPage }));
const LazyTokens = () => import('@/routes/TokensPage').then((m) => ({ default: m.TokensPage }));

function RequireAuth({ children }: { children: preact.ComponentChildren }) {
  const token = loadToken();
  if (!token) return <LoginPage />;
  return <>{children}</>;
}

function RequireCapability({ capability, children }: { capability: string; children: preact.ComponentChildren }) {
  const token = loadToken();
  if (!token) return <LoginPage />;
  // Bearer tokens carry the granted capability in the token itself.
  if (token.role === 'bearer' && token.scope !== capability) {
    return <p class="muted">This page requires the {capability} capability.</p>;
  }
  return <>{children}</>;
}

export function App() {
  return (
    <AppShell>
      <Router>
        <Switch>
          <Route path="/login" component={LoginPage} />
          <Route path="/" component={() => (
            <RequireAuth>
              <DashboardPage />
            </RequireAuth>
          )} />
          <Route path="/files" component={() => (
            <RequireAuth>
              <FilesPage />
            </RequireAuth>
          )} />
          <Route path="/grants" component={() => (
            <RequireCapability capability="grant:admin">
              <GrantsPage />
            </RequireCapability>
          )} />
          <Route path="/shares" component={() => (
            <RequireAuth>
              <SharesPage />
            </RequireAuth>
          )} />
          <Route path="/tokens" component={() => (
            <RequireCapability capability="grant:admin">
              <TokensPage />
            </RequireCapability>
          )} />
          <Route>
            <p class="muted">Page not found.</p>
          </Route>
        </Switch>
      </Router>
    </AppShell>
  );
}
