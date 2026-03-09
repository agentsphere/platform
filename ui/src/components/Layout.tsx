import { useState, useEffect, useRef } from 'preact/hooks';
import { useAuth } from '../lib/auth';
import { NotificationBell } from './NotificationBell';

const NAV = [
  { href: '/', label: 'Dashboard' },
  { href: '/workspaces', label: 'Workspaces' },
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
  { href: '/admin/health', label: 'Health' },
];

const SETTINGS_NAV = [
  { href: '/settings/account', label: 'Account' },
  { href: '/settings/tokens', label: 'API Tokens' },
  { href: '/settings/provider-keys', label: 'Provider Keys' },
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

function UserMenu() {
  const { user, logout } = useAuth();
  const [open, setOpen] = useState(false);
  const menuRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    const handleClick = (e: MouseEvent) => {
      if (menuRef.current && !menuRef.current.contains(e.target as Node)) {
        setOpen(false);
      }
    };
    document.addEventListener('click', handleClick);
    return () => document.removeEventListener('click', handleClick);
  }, [open]);

  return (
    <div class="user-menu" ref={menuRef}>
      <button class="btn btn-ghost btn-sm" onClick={(e) => { e.stopPropagation(); setOpen(!open); }}>
        {user?.display_name || user?.name}
      </button>
      {open && (
        <div class="user-menu-dropdown">
          <a href="/settings/account" class="user-menu-item">Account Settings</a>
          <a href="/settings/tokens" class="user-menu-item">API Tokens</a>
          <a href="/settings/provider-keys" class="user-menu-item">Provider Keys</a>
          <div class="user-menu-divider" />
          <button class="user-menu-item user-menu-logout" onClick={() => logout().then(() => { window.location.href = '/login'; })}>
            Logout
          </button>
        </div>
      )}
    </div>
  );
}

export function Layout({ children }: { children: any }) {
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
          <UserMenu />
        </header>
        <div class="content">
          {children}
        </div>
      </div>
    </div>
  );
}
