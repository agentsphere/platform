import { useAuth } from '../lib/auth';

const NAV = [
  { href: '/', label: 'Dashboard' },
  { href: '/projects', label: 'Projects' },
];

const ADMIN_NAV = [
  { href: '/admin/users', label: 'Users' },
  { href: '/admin/roles', label: 'Roles' },
  { href: '/admin/delegations', label: 'Delegations' },
];

const SETTINGS_NAV = [
  { href: '/settings/tokens', label: 'API Tokens' },
];

function NavLink({ href, label }: { href: string; label: string }) {
  const active = window.location.pathname === href ||
    (href !== '/' && window.location.pathname.startsWith(href));
  return (
    <a href={href} class={`sidebar-link${active ? ' active' : ''}`}>
      {label}
    </a>
  );
}

export function Layout({ children }: { children: any }) {
  const { user, logout } = useAuth();

  return (
    <div class="layout">
      <nav class="sidebar">
        <div class="sidebar-brand">Platform</div>
        <div class="sidebar-section">
          {NAV.map(n => <NavLink key={n.href} href={n.href} label={n.label} />)}
        </div>
        <div class="sidebar-section">
          <div class="sidebar-label">Admin</div>
          {ADMIN_NAV.map(n => <NavLink key={n.href} href={n.href} label={n.label} />)}
        </div>
        <div class="sidebar-section">
          <div class="sidebar-label">Settings</div>
          {SETTINGS_NAV.map(n => <NavLink key={n.href} href={n.href} label={n.label} />)}
        </div>
        <div class="sidebar-spacer" />
      </nav>
      <div class="main">
        <header class="topbar">
          <span class="topbar-user">{user?.display_name || user?.name}</span>
          <button class="btn btn-ghost btn-sm" onClick={() => logout().then(() => { window.location.href = '/login'; })}>
            Logout
          </button>
        </header>
        <div class="content">
          {children}
        </div>
      </div>
    </div>
  );
}
