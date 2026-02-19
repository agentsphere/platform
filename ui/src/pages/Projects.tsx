import { useState, useEffect } from 'preact/hooks';
import { api, qs, type ListResponse } from '../lib/api';
import type { Project } from '../lib/types';
import { timeAgo } from '../lib/format';
import { Badge } from '../components/Badge';
import { Pagination } from '../components/Pagination';
import { Modal } from '../components/Modal';

const LIMIT = 20;

export function Projects() {
  const [projects, setProjects] = useState<Project[]>([]);
  const [total, setTotal] = useState(0);
  const [offset, setOffset] = useState(0);
  const [search, setSearch] = useState('');
  const [showCreate, setShowCreate] = useState(false);
  const [form, setForm] = useState({ name: '', description: '', visibility: 'private' });
  const [error, setError] = useState('');

  const load = () => {
    api.get<ListResponse<Project>>(`/api/projects${qs({ limit: LIMIT, offset, search: search || undefined })}`)
      .then(r => { setProjects(r.items); setTotal(r.total); })
      .catch(() => {});
  };

  useEffect(load, [offset, search]);

  const create = async (e: Event) => {
    e.preventDefault();
    setError('');
    try {
      const p = await api.post<Project>('/api/projects', form);
      setShowCreate(false);
      setForm({ name: '', description: '', visibility: 'private' });
      window.location.href = `/projects/${p.id}`;
    } catch (err: any) {
      setError(err.message || 'Failed to create project');
    }
  };

  return (
    <div>
      <div class="flex-between mb-md">
        <h2>Projects</h2>
        <button class="btn btn-primary" onClick={() => setShowCreate(true)}>New Project</button>
      </div>
      <div class="mb-md">
        <input class="input" placeholder="Search projects..." value={search}
          onInput={(e) => { setSearch((e.target as HTMLInputElement).value); setOffset(0); }} />
      </div>
      <div class="card">
        {projects.length === 0 ? (
          <div class="empty-state">No projects found</div>
        ) : (
          <table class="table">
            <thead>
              <tr><th>Name</th><th>Visibility</th><th>Branch</th><th>Updated</th></tr>
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
        <Pagination total={total} limit={LIMIT} offset={offset} onChange={setOffset} />
      </div>

      <Modal open={showCreate} onClose={() => setShowCreate(false)} title="New Project">
        <form onSubmit={create}>
          <div class="form-group">
            <label>Name</label>
            <input class="input" required value={form.name}
              onInput={(e) => setForm({ ...form, name: (e.target as HTMLInputElement).value })} />
          </div>
          <div class="form-group">
            <label>Description</label>
            <textarea class="input" value={form.description}
              onInput={(e) => setForm({ ...form, description: (e.target as HTMLTextAreaElement).value })} />
          </div>
          <div class="form-group">
            <label>Visibility</label>
            <select class="input" value={form.visibility}
              onChange={(e) => setForm({ ...form, visibility: (e.target as HTMLSelectElement).value })}>
              <option value="private">Private</option>
              <option value="internal">Internal</option>
              <option value="public">Public</option>
            </select>
          </div>
          {error && <div class="error-msg">{error}</div>}
          <div class="modal-actions">
            <button type="button" class="btn" onClick={() => setShowCreate(false)}>Cancel</button>
            <button type="submit" class="btn btn-primary">Create</button>
          </div>
        </form>
      </Modal>
    </div>
  );
}
