import { useState, useEffect } from 'preact/hooks';
import { api, type ListResponse } from '../lib/api';
import type { AuditLogEntry } from '../lib/types';
import { timeAgo } from '../lib/format';

export function ActivityFeed() {
  const [entries, setEntries] = useState<AuditLogEntry[]>([]);

  useEffect(() => {
    api.get<ListResponse<AuditLogEntry>>('/api/audit-log?limit=15')
      .then(r => setEntries(r.items))
      .catch(() => {}); // hidden for non-admins
  }, []);

  if (entries.length === 0) return null;

  const icon = (action: string) => {
    if (action.startsWith('project.')) return '📁';
    if (action.startsWith('deploy.') || action.startsWith('release.')) return '🚀';
    if (action.startsWith('pipeline.') || action.startsWith('build.')) return '🔨';
    if (action.startsWith('user.') || action.startsWith('auth.')) return '👤';
    if (action.startsWith('session.') || action.startsWith('agent.')) return '🤖';
    if (action.startsWith('secret.')) return '🔑';
    if (action.startsWith('flag.')) return '🚩';
    return '📋';
  };

  const shortAction = (action: string) => action.replace(/\./g, ' ').replace(/_/g, ' ');

  return (
    <div class="panel">
      <div class="panel-header">Activity</div>
      <div class="panel-body activity-feed">
        {entries.map(e => (
          <div key={e.id} class="activity-item">
            <span class="activity-icon">{icon(e.action)}</span>
            <div class="activity-content">
              <span class="activity-actor">{e.actor_name}</span>
              {' '}
              <span class="activity-action">{shortAction(e.action)}</span>
              <div class="activity-time">{timeAgo(e.created_at)}</div>
            </div>
          </div>
        ))}
      </div>
    </div>
  );
}
