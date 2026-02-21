import { useState } from 'preact/hooks';
import { useAuth } from '../lib/auth';
import { NotificationBell } from './NotificationBell';

const NAV = [
  { href: '/', label: 'Dashboard' },
  { href: '/projects', label: 'Projects' },
];

const OBSERVE_NAV = [
  { href: '/observe/logs', label: 'Logs' },
  { href: '/observe/traces', label: 'Traces' },
  { href: '/observe/metrics', label: 'Metrics' },
  { href: '/observe/alerts', label: 'Alerts' },
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
  const [sidebarOpen, setSidebarOpen] = useState(false);

  return (
    <div class="layout">
      <button class="sidebar-toggle" onClick={() => setSidebarOpen(!sidebarOpen)}
        aria-label="Toggle sidebar">
        <span class="sidebar-toggle-bar" />
        <span class="sidebar-toggle-bar" />
        <span class="sidebar-toggle-bar" />
      </button>
      <nav class={`sidebar ${sidebarOpen ? 'sidebar-open' : ''}`}>
        <div class="sidebar-brand">Platform</div>
        <div class="sidebar-section">
          {NAV.map(n => <NavLink key={n.href} href={n.href} label={n.label} />)}
        </div>
        <div class="sidebar-section">
          <div class="sidebar-label">Observe</div>
          {OBSERVE_NAV.map(n => <NavLink key={n.href} href={n.href} label={n.label} />)}
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
      {sidebarOpen && <div class="sidebar-overlay" onClick={() => setSidebarOpen(false)} />}
      <div class="main">
        <header class="topbar">
          <NotificationBell />
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
