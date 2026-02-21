import { useState, useEffect, useRef } from 'preact/hooks';
import { api, type ListResponse } from '../lib/api';
import type { Notification } from '../lib/types';
import { timeAgo } from '../lib/format';

export function NotificationBell() {
  const [unreadCount, setUnreadCount] = useState(0);
  const [notifications, setNotifications] = useState<Notification[]>([]);
  const [open, setOpen] = useState(false);
  const dropdownRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    loadCount();
    const interval = setInterval(loadCount, 30000);
    return () => clearInterval(interval);
  }, []);

  useEffect(() => {
    if (!open) return;
    const handleClick = (e: MouseEvent) => {
      if (dropdownRef.current && !dropdownRef.current.contains(e.target as Node)) {
        setOpen(false);
      }
    };
    document.addEventListener('click', handleClick);
    return () => document.removeEventListener('click', handleClick);
  }, [open]);

  const loadCount = () => {
    api.get<{ count: number }>('/api/notifications/unread-count')
      .then(r => setUnreadCount(r.count))
      .catch(() => {});
  };

  const toggle = (e: Event) => {
    e.stopPropagation();
    if (!open) {
      api.get<ListResponse<Notification>>('/api/notifications?limit=5&status=unread')
        .then(r => setNotifications(r.items))
        .catch(() => {});
    }
    setOpen(!open);
  };

  const markRead = async (id: string) => {
    await api.patch(`/api/notifications/${id}`, { status: 'read' });
    setNotifications(prev => prev.filter(n => n.id !== id));
    setUnreadCount(prev => Math.max(0, prev - 1));
  };

  const getNotificationHref = (n: Notification): string | null => {
    if (n.ref_type === 'project' && n.ref_id) return `/projects/${n.ref_id}`;
    if (n.ref_type === 'issue' && n.ref_id) return `/projects/${n.ref_id}`;
    return null;
  };

  return (
    <div class="notification-bell" ref={dropdownRef}>
      <button class="btn btn-ghost btn-sm notification-bell-btn" onClick={toggle}>
        <span class="bell-icon">Bell</span>
        {unreadCount > 0 && (
          <span class="notification-badge">{unreadCount > 99 ? '99+' : unreadCount}</span>
        )}
      </button>

      {open && (
        <div class="notification-dropdown">
          <div class="notification-dropdown-header">
            <span class="text-sm" style="font-weight:600">Notifications</span>
          </div>
          {notifications.length === 0 ? (
            <div class="notification-empty">No unread notifications</div>
          ) : (
            notifications.map(n => {
              const href = getNotificationHref(n);
              return (
                <div key={n.id} class="notification-item"
                  onClick={() => {
                    markRead(n.id);
                    if (href) window.location.href = href;
                  }}>
                  <div class="notification-subject">{n.subject}</div>
                  {n.body && <div class="notification-body">{n.body}</div>}
                  <div class="notification-time">{timeAgo(n.created_at)}</div>
                </div>
              );
            })
          )}
          <div class="notification-dropdown-footer">
            <a href="/settings/notifications" class="text-xs">View all notifications</a>
          </div>
        </div>
      )}
    </div>
  );
}
