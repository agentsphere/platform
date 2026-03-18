import { useState, useEffect, useRef } from 'preact/hooks';
import { useRouter } from 'preact-router';
import { useAuth } from '../lib/auth';
import { NotificationCenter } from './NotificationCenter';

// SVG icon paths (stroke-based, 24x24 viewBox)
const ICONS: Record<string, string> = {
  home: '<path d="M3 12l2-2m0 0l7-7 7 7M5 10v10a1 1 0 001 1h3m10-11l2 2m-2-2v10a1 1 0 01-1 1h-3m-4 0a1 1 0 01-1-1v-4a1 1 0 011-1h2a1 1 0 011 1v4a1 1 0 01-1 1"/>',
  plus: '<path d="M12 4v16m8-8H4"/>',
  folder: '<path d="M3 7v10a2 2 0 002 2h14a2 2 0 002-2V9a2 2 0 00-2-2h-6l-2-2H5a2 2 0 00-2 2z"/>',
  building: '<path d="M19 21V5a2 2 0 00-2-2H7a2 2 0 00-2 2v16m14 0H5m14 0h2m-16 0H3"/><path d="M9 7h1m4 0h1m-6 4h1m4 0h1m-6 4h1m4 0h1"/>',
  log: '<path d="M14 2H6a2 2 0 00-2 2v16a2 2 0 002 2h12a2 2 0 002-2V8z"/><path d="M14 2v6h6"/><path d="M16 13H8m8 4H8m2-8H8"/>',
  trace: '<path d="M22 12h-4l-3 9L9 3l-3 9H2"/>',
  chart: '<path d="M18 20V10m-6 10V4M6 20v-6"/>',
  bell: '<path d="M18 8A6 6 0 006 8c0 7-3 9-3 9h18s-3-2-3-9"/><path d="M13.73 21a2 2 0 01-3.46 0"/>',
  users: '<path d="M17 21v-2a4 4 0 00-4-4H5a4 4 0 00-4 4v2"/><circle cx="9" cy="7" r="4"/><path d="M23 21v-2a4 4 0 00-3-3.87"/><path d="M16 3.13a4 4 0 010 7.75"/>',
  shield: '<path d="M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10z"/>',
  link: '<path d="M10 13a5 5 0 007.54.54l3-3a5 5 0 00-7.07-7.07l-1.72 1.71"/><path d="M14 11a5 5 0 00-7.54-.54l-3 3a5 5 0 007.07 7.07l1.71-1.71"/>',
  heart: '<path d="M20.84 4.61a5.5 5.5 0 00-7.78 0L12 5.67l-1.06-1.06a5.5 5.5 0 00-7.78 7.78l1.06 1.06L12 21.23l7.78-7.78 1.06-1.06a5.5 5.5 0 000-7.78z"/>',
  settings: '<circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.65 1.65 0 00.33 1.82l.06.06a2 2 0 010 2.83 2 2 0 01-2.83 0l-.06-.06a1.65 1.65 0 00-1.82-.33 1.65 1.65 0 00-1 1.51V21a2 2 0 01-4 0v-.09A1.65 1.65 0 009 19.4a1.65 1.65 0 00-1.82.33l-.06.06a2 2 0 01-2.83-2.83l.06-.06A1.65 1.65 0 004.68 15a1.65 1.65 0 00-1.51-1H3a2 2 0 010-4h.09A1.65 1.65 0 004.6 9a1.65 1.65 0 00-.33-1.82l-.06-.06a2 2 0 012.83-2.83l.06.06A1.65 1.65 0 009 4.68a1.65 1.65 0 001-1.51V3a2 2 0 014 0v.09a1.65 1.65 0 001 1.51 1.65 1.65 0 001.82-.33l.06-.06a2 2 0 012.83 2.83l-.06.06A1.65 1.65 0 0019.4 9a1.65 1.65 0 001.51 1H21a2 2 0 010 4h-.09a1.65 1.65 0 00-1.51 1z"/>',
  key: '<path d="M21 2l-2 2m-7.61 7.61a5.5 5.5 0 11-7.78 7.78 5.5 5.5 0 017.78-7.78zm0 0L15.5 7.5m0 0l3 3L22 7l-3-3m-3.5 3.5L19 4"/>',
  token: '<rect x="3" y="11" width="18" height="11" rx="2" ry="2"/><path d="M7 11V7a5 5 0 0110 0v4"/>',
  sun: '<circle cx="12" cy="12" r="5"/><line x1="12" y1="1" x2="12" y2="3"/><line x1="12" y1="21" x2="12" y2="23"/><line x1="4.22" y1="4.22" x2="5.64" y2="5.64"/><line x1="18.36" y1="18.36" x2="19.78" y2="19.78"/><line x1="1" y1="12" x2="3" y2="12"/><line x1="21" y1="12" x2="23" y2="12"/><line x1="4.22" y1="19.78" x2="5.64" y2="18.36"/><line x1="18.36" y1="5.64" x2="19.78" y2="4.22"/>',
  moon: '<path d="M21 12.79A9 9 0 1111.21 3 7 7 0 0021 12.79z"/>',
};

function SvgIcon({ name }: { name: string }) {
  const path = ICONS[name] || ICONS.folder;
  return (
    <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor"
      stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"
      dangerouslySetInnerHTML={{ __html: path }} />
  );
}

interface NavItem { href: string; label: string; icon: string }

const NAV: NavItem[] = [
  { href: '/', label: 'Home', icon: 'home' },
  { href: '/create-app', label: 'New App', icon: 'plus' },
  { href: '/projects', label: 'Projects', icon: 'folder' },
  { href: '/workspaces', label: 'Workspaces', icon: 'building' },
];

const OBSERVE_NAV: NavItem[] = [
  { href: '/observe/logs', label: 'Logs', icon: 'log' },
  { href: '/observe/traces', label: 'Traces', icon: 'trace' },
  { href: '/observe/metrics', label: 'Metrics', icon: 'chart' },
  { href: '/observe/alerts', label: 'Alerts', icon: 'bell' },
];

const ADMIN_NAV: NavItem[] = [
  { href: '/admin/users', label: 'Users', icon: 'users' },
  { href: '/admin/roles', label: 'Roles', icon: 'shield' },
  { href: '/admin/delegations', label: 'Delegations', icon: 'link' },
  { href: '/admin/skills', label: 'Skills', icon: 'key' },
  { href: '/admin/health', label: 'Health', icon: 'heart' },
];

function NavItemLink({ item, currentPath }: { item: NavItem; currentPath: string }) {
  const active = currentPath === item.href ||
    (item.href !== '/' && currentPath.startsWith(item.href));
  return (
    <a href={item.href} class={`sidebar-v2-item${active ? ' active' : ''}`}>
      <span class="sidebar-v2-icon"><SvgIcon name={item.icon} /></span>
      <span class="sidebar-v2-item-label">{item.label}</span>
    </a>
  );
}

function SidebarUser() {
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

  const displayName = user?.display_name || user?.name || '?';
  const initial = displayName.charAt(0).toUpperCase();

  return (
    <div style="position:relative" ref={menuRef}>
      <div class="sidebar-v2-user" onClick={(e) => { e.stopPropagation(); setOpen(!open); }}>
        <div class="sidebar-v2-avatar">{initial}</div>
        <div class="sidebar-v2-user-info">
          <div class="sidebar-v2-user-name">{displayName}</div>
          <div class="sidebar-v2-user-email">{user?.email}</div>
        </div>
      </div>
      {open && (
        <div class="sidebar-v2-dropdown">
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

function getInitialTheme(): 'light' | 'dark' {
  const stored = localStorage.getItem('theme');
  if (stored === 'light' || stored === 'dark') return stored;
  return window.matchMedia('(prefers-color-scheme: light)').matches ? 'light' : 'dark';
}

function ThemeToggle() {
  const [theme, setTheme] = useState<'light' | 'dark'>(getInitialTheme);

  useEffect(() => {
    document.documentElement.setAttribute('data-theme', theme);
    localStorage.setItem('theme', theme);
  }, [theme]);

  const toggle = () => setTheme(t => t === 'dark' ? 'light' : 'dark');

  return (
    <button class="sidebar-v2-item" onClick={toggle} style="border:none;background:none;cursor:pointer;width:100%">
      <span class="sidebar-v2-icon">
        <SvgIcon name={theme === 'dark' ? 'sun' : 'moon'} />
      </span>
      <span class="sidebar-v2-item-label">{theme === 'dark' ? 'Light Mode' : 'Dark Mode'}</span>
    </button>
  );
}

export function Layout({ children }: { children: any }) {
  const [sidebarOpen, setSidebarOpen] = useState(false);
  const [routeMatch] = useRouter();
  const currentPath = routeMatch?.url || '/';

  // Apply stored theme on mount (prevents flash)
  useEffect(() => {
    document.documentElement.setAttribute('data-theme', getInitialTheme());
  }, []);

  return (
    <div class="layout">
      {/* Mobile hamburger */}
      <button class="sidebar-toggle" onClick={() => setSidebarOpen(!sidebarOpen)}
        aria-label="Toggle sidebar">
        <span class="sidebar-toggle-bar" />
        <span class="sidebar-toggle-bar" />
        <span class="sidebar-toggle-bar" />
      </button>

      {/* Icon sidebar */}
      <nav class={`sidebar-v2${sidebarOpen ? ' sidebar-open' : ''}`}>
        <div class="sidebar-v2-brand">
          <span class="sidebar-v2-icon">
            <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="var(--accent)" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
              <polygon points="12 2 2 7 12 12 22 7 12 2" />
              <polyline points="2 17 12 22 22 17" />
              <polyline points="2 12 12 17 22 12" />
            </svg>
          </span>
          <span class="sidebar-v2-brand-text">Platform</span>
        </div>

        <div class="sidebar-v2-section">
          {NAV.map(n => <NavItemLink key={n.href} item={n} currentPath={currentPath} />)}
        </div>

        <div class="sidebar-v2-section">
          <div class="sidebar-v2-label">Observe</div>
          {OBSERVE_NAV.map(n => <NavItemLink key={n.href} item={n} currentPath={currentPath} />)}
        </div>

        <div class="sidebar-v2-section">
          <div class="sidebar-v2-label">Admin</div>
          {ADMIN_NAV.map(n => <NavItemLink key={n.href} item={n} currentPath={currentPath} />)}
        </div>

        <div class="sidebar-v2-spacer" />

        {/* Settings + theme at bottom */}
        <div class="sidebar-v2-section">
          <NavItemLink item={{ href: '/settings/account', label: 'Settings', icon: 'settings' }} currentPath={currentPath} />
          <ThemeToggle />
        </div>

        <SidebarUser />
      </nav>

      {sidebarOpen && <div class="sidebar-overlay" onClick={() => setSidebarOpen(false)} />}

      {/* Notification center — fixed top center */}
      <NotificationCenter />

      <div class="main-v2">
        <div class="content-v2">
          {children}
        </div>
      </div>
    </div>
  );
}
