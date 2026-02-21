import { useState, useEffect } from 'preact/hooks';
import { api, type ListResponse } from '../lib/api';
import type { Project, AuditLogEntry } from '../lib/types';
import { timeAgo } from '../lib/format';
import { Badge } from '../components/Badge';

interface DashboardStats {
  projects: number;
  active_sessions: number;
  running_builds: number;
  failed_builds: number;
  healthy_deployments: number;
  degraded_deployments: number;
}

export function Dashboard() {
  const [projects, setProjects] = useState<Project[]>([]);
  const [total, setTotal] = useState(0);
  const [stats, setStats] = useState<DashboardStats | null>(null);
  const [activity, setActivity] = useState<AuditLogEntry[]>([]);

  useEffect(() => {
    api.get<ListResponse<Project>>('/api/projects?limit=10')
      .then(r => { setProjects(r.items); setTotal(r.total); })
      .catch(() => {});

    // Load dashboard stats
    api.get<DashboardStats>('/api/dashboard/stats')
      .then(setStats)
      .catch(() => {
        // Fallback: build stats from project count
        setStats({ projects: 0, active_sessions: 0, running_builds: 0, failed_builds: 0, healthy_deployments: 0, degraded_deployments: 0 });
      });

    // Load recent activity
    api.get<ListResponse<AuditLogEntry>>('/api/audit-log?limit=10')
      .then(r => setActivity(r.items))
      .catch(() => setActivity([]));
  }, []);

  return (
    <div>
      <h2 style="margin-bottom:1rem">Dashboard</h2>

      {/* Status summary cards */}
      <div class="stats-grid mb-md">
        <div class="stat-card">
          <div class="stat-value">{stats?.projects ?? total}</div>
          <div class="stat-label">Projects</div>
        </div>
        <div class="stat-card">
          <div class="stat-value">{stats?.active_sessions ?? 0}</div>
          <div class="stat-label">Active Sessions</div>
        </div>
        <div class="stat-card">
          <div class="stat-value">
            {stats?.running_builds ?? 0}
            {(stats?.failed_builds ?? 0) > 0 && (
              <span class="stat-sub" style="color:var(--danger)"> / {stats?.failed_builds} failed</span>
            )}
          </div>
          <div class="stat-label">Running Builds</div>
        </div>
        <div class="stat-card">
          <div class="stat-value">
            {stats?.healthy_deployments ?? 0}
            {(stats?.degraded_deployments ?? 0) > 0 && (
              <span class="stat-sub" style="color:var(--warning)"> / {stats?.degraded_deployments} degraded</span>
            )}
          </div>
          <div class="stat-label">Deployments</div>
        </div>
      </div>

      {/* Quick actions */}
      <div class="flex gap-sm mb-md">
        <a href="/projects" class="btn btn-primary btn-sm">New Project</a>
        <a href="/observe/logs" class="btn btn-sm">View Logs</a>
        <a href="/observe/traces" class="btn btn-sm">View Traces</a>
        <a href="/observe/metrics" class="btn btn-sm">View Metrics</a>
      </div>

      {/* Recent activity */}
      {activity.length > 0 && (
        <div class="card mb-md">
          <div class="card-header">
            <span class="card-title">Recent Activity</span>
          </div>
          <div class="activity-feed">
            {activity.map(entry => (
              <div key={entry.id} class="activity-item">
                <span class="activity-actor">{entry.actor_name}</span>
                <span class="activity-action">{formatAction(entry.action)}</span>
                {entry.resource && (
                  <span class="activity-resource">{entry.resource}</span>
                )}
                <span class="activity-time">{timeAgo(entry.created_at)}</span>
              </div>
            ))}
          </div>
        </div>
      )}

      {/* Recent projects */}
      <div class="card">
        <div class="card-header">
          <span class="card-title">Recent Projects</span>
          <span class="text-muted text-sm">{total} total</span>
        </div>
        {projects.length === 0 ? (
          <div class="empty-state">No projects yet</div>
        ) : (
          <table class="table">
            <thead>
              <tr>
                <th>Name</th>
                <th>Visibility</th>
                <th>Branch</th>
                <th>Updated</th>
              </tr>
            </thead>
            <tbody>
              {projects.map(p => (
                <tr key={p.id} class="table-link" onClick={() => { window.location.href = `/projects/${p.id}`; }}>
                  <td><a href={`/projects/${p.id}`}>{p.display_name || p.name}</a></td>
                  <td><Badge status={p.visibility} /></td>
                  <td class="mono text-sm">{p.default_branch}</td>
                  <td class="text-muted text-sm">{timeAgo(p.updated_at)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>
    </div>
  );
}

function formatAction(action: string): string {
  return action.replace(/\./g, ' ').replace(/_/g, ' ');
}
