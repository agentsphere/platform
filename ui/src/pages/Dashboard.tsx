import { useState, useEffect } from 'preact/hooks';
import { api, type ListResponse } from '../lib/api';
import type { Project } from '../lib/types';
import { timeAgo } from '../lib/format';
import { Badge } from '../components/Badge';

export function Dashboard() {
  const [projects, setProjects] = useState<Project[]>([]);
  const [total, setTotal] = useState(0);

  useEffect(() => {
    api.get<ListResponse<Project>>('/api/projects?limit=10')
      .then(r => { setProjects(r.items); setTotal(r.total); })
      .catch(() => {});
  }, []);

  return (
    <div>
      <h2 style="margin-bottom:1rem">Dashboard</h2>
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
