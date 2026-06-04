import type { ComponentChildren } from 'preact';

interface Props {
  children: ComponentChildren;
}

export function AppShell({ children }: Props) {
  return (
    <div id="app-shell">
      <header>
        <nav>
          <a href="/">Cascade</a>
        </nav>
      </header>
      <main>{children}</main>
    </div>
  );
}
