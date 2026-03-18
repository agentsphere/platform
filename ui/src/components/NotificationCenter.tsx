import { useState, useEffect, useRef } from 'preact/hooks';
import { api, type ListResponse } from '../lib/api';
import type { Notification } from '../lib/types';
import { timeAgo } from '../lib/format';

export function NotificationCenter() {
  const [unreadCount, setUnreadCount] = useState(0);
  const [notifications, setNotifications] = useState<Notification[]>([]);
  const [open, setOpen] = useState(false);
  const panelRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    loadCount();
    const interval = setInterval(loadCount, 30000);
    return () => clearInterval(interval);
  }, []);

  useEffect(() => {
    if (!open) return;
    const handleClick = (e: MouseEvent) => {
      if (panelRef.current && !panelRef.current.contains(e.target as Node)) {
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
    await api.patch(`/api/notifications/${id}/read`, {});
    setNotifications(prev => prev.filter(n => n.id !== id));
    setUnreadCount(prev => Math.max(0, prev - 1));
  };

  const getNotificationHref = (n: Notification): string | null => {
    if (n.ref_type === 'project' && n.ref_id) return `/projects/${n.ref_id}`;
    if (n.ref_type === 'issue' && n.ref_id) return `/projects/${n.ref_id}`;
    return null;
  };

  return (
    <div ref={panelRef}>
      <div class="notification-center-trigger" onClick={toggle}>
        {unreadCount > 0 ? (
          <>
            <span class="notification-center-dot" />
            <span class="notification-center-count">{unreadCount > 99 ? '99+' : unreadCount}</span>
          </>
        ) : (
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
            <path d="M18 8A6 6 0 0 0 6 8c0 7-3 9-3 9h18s-3-2-3-9" />
            <path d="M13.73 21a2 2 0 0 1-3.46 0" />
          </svg>
        )}
      </div>

      {open && (
        <div class="notification-center-panel">
          <div class="notification-center-panel-header">Notifications</div>
          {notifications.length === 0 ? (
            <div class="notification-center-empty">
              <svg width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="var(--text-muted)" stroke-width="1.5" style="margin-bottom:0.5rem">
                <path d="M18 8A6 6 0 0 0 6 8c0 7-3 9-3 9h18s-3-2-3-9" />
                <path d="M13.73 21a2 2 0 0 1-3.46 0" />
              </svg>
              <div>No new notifications</div>
            </div>
          ) : (
            notifications.map(n => {
              const href = getNotificationHref(n);
              return (
                <div key={n.id} class="notification-center-item"
                  onClick={() => {
                    markRead(n.id);
                    if (href) window.location.href = href;
                  }}>
                  <div class="notification-subject" style="font-size:13px;font-weight:500;color:var(--text-primary);margin-bottom:0.15rem">{n.subject}</div>
                  {n.body && <div style="font-size:12px;color:var(--text-secondary);margin-bottom:0.15rem">{n.body}</div>}
                  <div style="font-size:11px;color:var(--text-muted)">{timeAgo(n.created_at)}</div>
                </div>
              );
            })
          )}
          <div class="notification-center-panel-footer">
            <a href="/settings/notifications" class="text-xs">View all</a>
          </div>
        </div>
      )}
    </div>
  );
}
