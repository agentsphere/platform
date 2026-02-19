import { useState, useEffect } from 'preact/hooks';
import { api, qs, type ListResponse } from '../lib/api';
import type { Project, Issue, MergeRequest, Pipeline, Deployment, Webhook, TreeEntry, BlobResponse, BranchInfo } from '../lib/types';
import { timeAgo } from '../lib/format';
import { Badge } from '../components/Badge';
import { Pagination } from '../components/Pagination';
import { Modal } from '../components/Modal';

interface Props { id?: string; tab?: string; }

const TABS = ['files', 'issues', 'mrs', 'builds', 'deployments', 'webhooks', 'settings'];

export function ProjectDetail({ id, tab }: Props) {
  const [project, setProject] = useState<Project | null>(null);
  const currentTab = tab || 'files';

  useEffect(() => {
    if (id) api.get<Project>(`/api/projects/${id}`).then(setProject).catch(() => {});
  }, [id]);

  if (!project) return <div class="empty-state">Loading...</div>;

  return (
    <div>
      <div class="flex-between mb-md">
        <div>
          <h2>{project.display_name || project.name}</h2>
          {project.description && <p class="text-muted text-sm mt-sm">{project.description}</p>}
        </div>
        <Badge status={project.visibility} />
      </div>
      <div class="tabs">
        {TABS.map(t => (
          <a key={t} class={`tab${currentTab === t ? ' active' : ''}`}
            href={`/projects/${id}/${t}`}>{t === 'mrs' ? 'MRs' : t[0].toUpperCase() + t.slice(1)}</a>
        ))}
      </div>
      {currentTab === 'files' && <FilesTab projectId={id!} defaultBranch={project.default_branch} />}
      {currentTab === 'issues' && <IssuesTab projectId={id!} />}
      {currentTab === 'mrs' && <MRsTab projectId={id!} />}
      {currentTab === 'builds' && <BuildsTab projectId={id!} />}
      {currentTab === 'deployments' && <DeploymentsTab projectId={id!} />}
      {currentTab === 'webhooks' && <WebhooksTab projectId={id!} />}
      {currentTab === 'settings' && <SettingsTab project={project} onUpdate={setProject} />}
    </div>
  );
}

function FilesTab({ projectId, defaultBranch }: { projectId: string; defaultBranch: string }) {
  const [branches, setBranches] = useState<BranchInfo[]>([]);
  const [gitRef, setRef] = useState(defaultBranch);
  const [path, setPath] = useState('');
  const [entries, setEntries] = useState<TreeEntry[]>([]);
  const [blob, setBlob] = useState<BlobResponse | null>(null);

  useEffect(() => {
    api.get<BranchInfo[]>(`/api/projects/${projectId}/branches`).then(setBranches).catch(() => {});
  }, [projectId]);

  useEffect(() => {
    setBlob(null);
    api.get<TreeEntry[]>(`/api/projects/${projectId}/tree${qs({ ref: gitRef, path })}`)
      .then(setEntries)
      .catch(() => setEntries([]));
  }, [projectId, gitRef, path]);

  const openEntry = (entry: TreeEntry) => {
    if (entry.entry_type === 'tree') {
      setPath(path ? `${path}/${entry.name}` : entry.name);
    } else {
      const filePath = path ? `${path}/${entry.name}` : entry.name;
      api.get<BlobResponse>(`/api/projects/${projectId}/blob${qs({ ref: gitRef, path: filePath })}`)
        .then(setBlob).catch(() => {});
    }
  };

  if (blob) {
    return (
      <div class="card">
        <div class="flex-between mb-md">
          <span class="mono text-sm">{blob.path}</span>
          <button class="btn btn-sm" onClick={() => setBlob(null)}>Back</button>
        </div>
        <pre class="log-viewer">{blob.encoding === 'base64' ? atob(blob.content) : blob.content}</pre>
      </div>
    );
  }

  return (
    <div class="card">
      <div class="flex gap-sm mb-md">
        <select class="input" style="width:auto" value={gitRef}
          onChange={(e) => { setRef((e.target as HTMLSelectElement).value); setPath(''); }}>
          {branches.map(b => <option key={b.name} value={b.name}>{b.name}</option>)}
        </select>
        {path && (
          <button class="btn btn-sm" onClick={() => {
            const parts = path.split('/');
            parts.pop();
            setPath(parts.join('/'));
          }}>.. (up)</button>
        )}
        {path && <span class="mono text-sm text-muted">{path}/</span>}
      </div>
      {entries.length === 0 ? (
        <div class="empty-state">No files</div>
      ) : (
        entries.sort((a, b) => (a.entry_type === b.entry_type ? a.name.localeCompare(b.name) : a.entry_type === 'tree' ? -1 : 1))
          .map(e => (
            <div key={e.name} class="tree-entry" onClick={() => openEntry(e)}>
              <span class="tree-icon">{e.entry_type === 'tree' ? '/' : ' '}</span>
              <span>{e.name}</span>
              {e.size != null && <span class="text-muted text-xs" style="margin-left:auto">{e.size}</span>}
            </div>
          ))
      )}
    </div>
  );
}

function IssuesTab({ projectId }: { projectId: string }) {
  const [issues, setIssues] = useState<Issue[]>([]);
  const [total, setTotal] = useState(0);
  const [offset, setOffset] = useState(0);
  const [status, setStatus] = useState('open');
  const [showCreate, setShowCreate] = useState(false);
  const [form, setForm] = useState({ title: '', body: '' });
  const [error, setError] = useState('');

  const load = () => {
    api.get<ListResponse<Issue>>(`/api/projects/${projectId}/issues${qs({ limit: 20, offset, status })}`)
      .then(r => { setIssues(r.items); setTotal(r.total); }).catch(() => {});
  };
  useEffect(load, [projectId, offset, status]);

  const create = async (e: Event) => {
    e.preventDefault();
    try {
      await api.post(`/api/projects/${projectId}/issues`, form);
      setShowCreate(false);
      setForm({ title: '', body: '' });
      load();
    } catch (err: any) { setError(err.message); }
  };

  return (
    <div>
      <div class="flex-between mb-md">
        <div class="flex gap-sm">
          {['open', 'closed'].map(s => (
            <button key={s} class={`btn btn-sm${status === s ? ' btn-primary' : ''}`}
              onClick={() => { setStatus(s); setOffset(0); }}>{s}</button>
          ))}
        </div>
        <button class="btn btn-primary btn-sm" onClick={() => setShowCreate(true)}>New Issue</button>
      </div>
      <div class="card">
        {issues.length === 0 ? <div class="empty-state">No issues</div> : (
          <table class="table">
            <thead><tr><th>#</th><th>Title</th><th>Status</th><th>Created</th></tr></thead>
            <tbody>
              {issues.map(i => (
                <tr key={i.id} class="table-link" onClick={() => { window.location.href = `/projects/${projectId}/issues/${i.number}`; }}>
                  <td class="text-muted">{i.number}</td>
                  <td>{i.title}</td>
                  <td><Badge status={i.status} /></td>
                  <td class="text-muted text-sm">{timeAgo(i.created_at)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
        <Pagination total={total} limit={20} offset={offset} onChange={setOffset} />
      </div>
      <Modal open={showCreate} onClose={() => setShowCreate(false)} title="New Issue">
        <form onSubmit={create}>
          <div class="form-group">
            <label>Title</label>
            <input class="input" required value={form.title}
              onInput={(e) => setForm({ ...form, title: (e.target as HTMLInputElement).value })} />
          </div>
          <div class="form-group">
            <label>Body (markdown)</label>
            <textarea class="input" value={form.body}
              onInput={(e) => setForm({ ...form, body: (e.target as HTMLTextAreaElement).value })} />
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

function MRsTab({ projectId }: { projectId: string }) {
  const [mrs, setMrs] = useState<MergeRequest[]>([]);
  const [total, setTotal] = useState(0);
  const [offset, setOffset] = useState(0);
  const [status, setStatus] = useState('open');
  const [showCreate, setShowCreate] = useState(false);
  const [branches, setBranches] = useState<BranchInfo[]>([]);
  const [form, setForm] = useState({ source_branch: '', target_branch: 'main', title: '', body: '' });
  const [error, setError] = useState('');

  const load = () => {
    api.get<ListResponse<MergeRequest>>(`/api/projects/${projectId}/merge-requests${qs({ limit: 20, offset, status })}`)
      .then(r => { setMrs(r.items); setTotal(r.total); }).catch(() => {});
  };
  useEffect(load, [projectId, offset, status]);

  const openCreate = () => {
    api.get<BranchInfo[]>(`/api/projects/${projectId}/branches`).then(setBranches).catch(() => {});
    setShowCreate(true);
  };

  const create = async (e: Event) => {
    e.preventDefault();
    try {
      await api.post(`/api/projects/${projectId}/merge-requests`, form);
      setShowCreate(false);
      setForm({ source_branch: '', target_branch: 'main', title: '', body: '' });
      load();
    } catch (err: any) { setError(err.message); }
  };

  return (
    <div>
      <div class="flex-between mb-md">
        <div class="flex gap-sm">
          {['open', 'closed', 'merged'].map(s => (
            <button key={s} class={`btn btn-sm${status === s ? ' btn-primary' : ''}`}
              onClick={() => { setStatus(s); setOffset(0); }}>{s}</button>
          ))}
        </div>
        <button class="btn btn-primary btn-sm" onClick={openCreate}>New MR</button>
      </div>
      <div class="card">
        {mrs.length === 0 ? <div class="empty-state">No merge requests</div> : (
          <table class="table">
            <thead><tr><th>#</th><th>Title</th><th>Branches</th><th>Status</th><th>Created</th></tr></thead>
            <tbody>
              {mrs.map(m => (
                <tr key={m.id} class="table-link" onClick={() => { window.location.href = `/projects/${projectId}/merge-requests/${m.number}`; }}>
                  <td class="text-muted">{m.number}</td>
                  <td>{m.title}</td>
                  <td class="mono text-xs">{m.source_branch} → {m.target_branch}</td>
                  <td><Badge status={m.status} /></td>
                  <td class="text-muted text-sm">{timeAgo(m.created_at)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
        <Pagination total={total} limit={20} offset={offset} onChange={setOffset} />
      </div>
      <Modal open={showCreate} onClose={() => setShowCreate(false)} title="New Merge Request">
        <form onSubmit={create}>
          <div class="form-group">
            <label>Source branch</label>
            <select class="input" value={form.source_branch}
              onChange={(e) => setForm({ ...form, source_branch: (e.target as HTMLSelectElement).value })}>
              <option value="">Select...</option>
              {branches.map(b => <option key={b.name} value={b.name}>{b.name}</option>)}
            </select>
          </div>
          <div class="form-group">
            <label>Target branch</label>
            <select class="input" value={form.target_branch}
              onChange={(e) => setForm({ ...form, target_branch: (e.target as HTMLSelectElement).value })}>
              {branches.map(b => <option key={b.name} value={b.name}>{b.name}</option>)}
            </select>
          </div>
          <div class="form-group">
            <label>Title</label>
            <input class="input" required value={form.title}
              onInput={(e) => setForm({ ...form, title: (e.target as HTMLInputElement).value })} />
          </div>
          <div class="form-group">
            <label>Description</label>
            <textarea class="input" value={form.body}
              onInput={(e) => setForm({ ...form, body: (e.target as HTMLTextAreaElement).value })} />
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

function BuildsTab({ projectId }: { projectId: string }) {
  const [pipelines, setPipelines] = useState<Pipeline[]>([]);
  const [total, setTotal] = useState(0);
  const [offset, setOffset] = useState(0);

  useEffect(() => {
    api.get<ListResponse<Pipeline>>(`/api/projects/${projectId}/pipelines${qs({ limit: 20, offset })}`)
      .then(r => { setPipelines(r.items); setTotal(r.total); }).catch(() => {});
  }, [projectId, offset]);

  return (
    <div class="card">
      {pipelines.length === 0 ? <div class="empty-state">No pipelines</div> : (
        <table class="table">
          <thead><tr><th>Ref</th><th>Trigger</th><th>Status</th><th>Created</th></tr></thead>
          <tbody>
            {pipelines.map(p => (
              <tr key={p.id} class="table-link" onClick={() => { window.location.href = `/projects/${projectId}/pipelines/${p.id}`; }}>
                <td class="mono text-sm">{p.git_ref}</td>
                <td class="text-sm">{p.trigger}</td>
                <td><Badge status={p.status} /></td>
                <td class="text-muted text-sm">{timeAgo(p.created_at)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      <Pagination total={total} limit={20} offset={offset} onChange={setOffset} />
    </div>
  );
}

function DeploymentsTab({ projectId }: { projectId: string }) {
  const [deployments, setDeployments] = useState<Deployment[]>([]);

  useEffect(() => {
    api.get<ListResponse<Deployment>>(`/api/projects/${projectId}/deployments?limit=50`)
      .then(r => setDeployments(r.items)).catch(() => {});
  }, [projectId]);

  return (
    <div class="card">
      {deployments.length === 0 ? <div class="empty-state">No deployments</div> : (
        <table class="table">
          <thead><tr><th>Environment</th><th>Image</th><th>Desired</th><th>Current</th><th>Deployed</th></tr></thead>
          <tbody>
            {deployments.map(d => (
              <tr key={d.id}>
                <td><Badge status={d.environment} /></td>
                <td class="mono text-xs truncate" style="max-width:200px">{d.image_ref}</td>
                <td><Badge status={d.desired_status} /></td>
                <td><Badge status={d.current_status} /></td>
                <td class="text-muted text-sm">{d.deployed_at ? timeAgo(d.deployed_at) : '—'}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  );
}

function WebhooksTab({ projectId }: { projectId: string }) {
  const [webhooks, setWebhooks] = useState<Webhook[]>([]);
  const [showCreate, setShowCreate] = useState(false);
  const [form, setForm] = useState({ url: '', events: ['push'], secret: '' });
  const [error, setError] = useState('');

  const load = () => {
    api.get<ListResponse<Webhook>>(`/api/projects/${projectId}/webhooks?limit=50`)
      .then(r => setWebhooks(r.items)).catch(() => {});
  };
  useEffect(load, [projectId]);

  const create = async (e: Event) => {
    e.preventDefault();
    try {
      await api.post(`/api/projects/${projectId}/webhooks`, {
        url: form.url,
        events: form.events,
        secret: form.secret || undefined,
      });
      setShowCreate(false);
      setForm({ url: '', events: ['push'], secret: '' });
      load();
    } catch (err: any) { setError(err.message); }
  };

  const remove = async (whId: string) => {
    await api.del(`/api/projects/${projectId}/webhooks/${whId}`);
    load();
  };

  return (
    <div>
      <div class="flex-between mb-md">
        <span />
        <button class="btn btn-primary btn-sm" onClick={() => setShowCreate(true)}>New Webhook</button>
      </div>
      <div class="card">
        {webhooks.length === 0 ? <div class="empty-state">No webhooks</div> : (
          <table class="table">
            <thead><tr><th>URL</th><th>Events</th><th>Active</th><th></th></tr></thead>
            <tbody>
              {webhooks.map(w => (
                <tr key={w.id}>
                  <td class="mono text-xs truncate" style="max-width:250px">{w.url}</td>
                  <td class="text-xs">{w.events.join(', ')}</td>
                  <td><Badge status={w.active ? 'active' : 'inactive'} /></td>
                  <td><button class="btn btn-danger btn-sm" onClick={() => remove(w.id)}>Delete</button></td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>
      <Modal open={showCreate} onClose={() => setShowCreate(false)} title="New Webhook">
        <form onSubmit={create}>
          <div class="form-group">
            <label>URL</label>
            <input class="input" type="url" required value={form.url}
              onInput={(e) => setForm({ ...form, url: (e.target as HTMLInputElement).value })} />
          </div>
          <div class="form-group">
            <label>Events (comma-separated)</label>
            <input class="input" value={form.events.join(',')}
              onInput={(e) => setForm({ ...form, events: (e.target as HTMLInputElement).value.split(',').map(s => s.trim()).filter(Boolean) })} />
          </div>
          <div class="form-group">
            <label>Secret (optional)</label>
            <input class="input" type="password" value={form.secret}
              onInput={(e) => setForm({ ...form, secret: (e.target as HTMLInputElement).value })} />
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

function SettingsTab({ project, onUpdate }: { project: Project; onUpdate: (p: Project) => void }) {
  const [form, setForm] = useState({
    display_name: project.display_name || '',
    description: project.description || '',
    visibility: project.visibility,
    default_branch: project.default_branch,
  });
  const [saved, setSaved] = useState(false);
  const [error, setError] = useState('');

  const save = async (e: Event) => {
    e.preventDefault();
    setError('');
    try {
      const updated = await api.patch<Project>(`/api/projects/${project.id}`, form);
      onUpdate(updated);
      setSaved(true);
      setTimeout(() => setSaved(false), 2000);
    } catch (err: any) { setError(err.message); }
  };

  return (
    <div class="card">
      <form onSubmit={save}>
        <div class="form-group">
          <label>Display Name</label>
          <input class="input" value={form.display_name}
            onInput={(e) => setForm({ ...form, display_name: (e.target as HTMLInputElement).value })} />
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
        <div class="form-group">
          <label>Default Branch</label>
          <input class="input" value={form.default_branch}
            onInput={(e) => setForm({ ...form, default_branch: (e.target as HTMLInputElement).value })} />
        </div>
        {error && <div class="error-msg">{error}</div>}
        {saved && <div style="color:var(--success);font-size:13px">Saved</div>}
        <button type="submit" class="btn btn-primary mt-sm">Save Settings</button>
      </form>
    </div>
  );
}
