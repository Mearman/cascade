import type { ComponentChildren } from 'preact';
import { Link, useLocation } from 'wouter-preact';

interface Props {
  children: ComponentChildren;
}

const NAV_LINKS = [
  { href: '/', label: 'Dashboard' },
  { href: '/files', label: 'Files' },
  { href: '/shares', label: 'Shares' },
  { href: '/grants', label: 'Grants' },
  { href: '/tokens', label: 'Tokens' },
  { href: '/settings', label: 'Settings' },
] as const;

export function AppShell({ children }: Props) {
  const [location] = useLocation();

  function isActive(href: string): boolean {
    if (href === '/') return location === '/';
    return location.startsWith(href);
  }

  return (
    <div id="app-shell">
      <header>
        <nav class="main-nav">
          <span class="nav-brand">Cascade</span>
          <ul class="nav-links">
            {NAV_LINKS.map(({ href, label }) => (
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
