import type { ComponentChildren } from 'preact';
import { Link, useLocation } from 'wouter-preact';
import { useContext } from 'preact/hooks';
import { AppContext } from '@/context';
import { RuntimeMode } from '@/wasm';

interface Props {
  children: ComponentChildren;
}

interface NavLink {
  href: string;
  label: string;
  connectedOnly: boolean;
}

const NAV_LINKS: readonly NavLink[] = [
  { href: '/', label: 'Dashboard', connectedOnly: false },
  { href: '/files', label: 'Files', connectedOnly: true },
  { href: '/shares', label: 'Shares', connectedOnly: true },
  { href: '/grants', label: 'Grants', connectedOnly: true },
  { href: '/tokens', label: 'Tokens', connectedOnly: true },
  { href: '/settings', label: 'Settings', connectedOnly: false },
] as const;

export function AppShell({ children }: Props) {
  const [location] = useLocation();
  const { mode } = useContext(AppContext);

  function isActive(href: string): boolean {
    if (href === '/') return location === '/';
    return location.startsWith(href);
  }

  const visibleLinks = NAV_LINKS.filter(
    (link) => !link.connectedOnly || mode === RuntimeMode.Connected,
  );

  return (
    <div id="app-shell">
      <header>
        <nav class="main-nav">
          <span class="nav-brand">Cascade</span>
          <ul class="nav-links">
            {visibleLinks.map(({ href, label }) => (
              <li key={href}>
                <Link href={href} class={isActive(href) ? 'nav-link active' : 'nav-link'}>
                  {label}
                </Link>
              </li>
            ))}
          </ul>
        </nav>
      </header>
      <main class="main-content">{children}</main>
    </div>
  );
}
