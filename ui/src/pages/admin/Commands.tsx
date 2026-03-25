import { useState, useEffect } from 'preact/hooks';
import { api, qs, type ListResponse } from '../../lib/api';
import { timeAgo } from '../../lib/format';
import { Badge } from '../../components/Badge';
import { Pagination } from '../../components/Pagination';
import { Modal } from '../../components/Modal';
import { Markdown } from '../../components/Markdown';

interface Command {
  id: string;
  project_id: string | null;
  workspace_id: string | null;
  name: string;
  description: string;
  persistent_session: boolean;
  created_at: string;
  updated_at: string;
}

const LIMIT = 20;

export function Commands() {
  const [commands, setCommands] = useState<Command[]>([]);
  const [total, setTotal] = useState(0);
  const [offset, setOffset] = useState(0);
  const [showCreate, setShowCreate] = useState(false);
  const [editCmd, setEditCmd] = useState<Command | null>(null);
  const [createForm, setCreateForm] = useState({ name: '', description: '', prompt_template: '', persistent_session: false });
  const [editForm, setEditForm] = useState({ description: '', prompt_template: '', persistent_session: false });
  const [error, setError] = useState('');
  const [showPreview, setShowPreview] = useState(false);

  const load = () => {
    api.get<ListResponse<Command>>(`/api/commands${qs({ limit: LIMIT, offset })}`)
      .then(r => { setCommands(r.items); setTotal(r.total); }).catch(e => console.warn(e));
  };
  useEffect(load, [offset]);

  const create = async (e: Event) => {
    e.preventDefault();
    setError('');
    try {
      await api.post('/api/commands', createForm);
      setShowCreate(false);
      setCreateForm({ name: '', description: '', prompt_template: '', persistent_session: false });
      load();
    } catch (err: any) { setError(err.message); }
  };

  const openEdit = (cmd: Command) => {
    setEditCmd(cmd);
    setError('');
    // Fetch full command to get prompt_template
    api.get<Command & { prompt_template?: string }>(`/api/commands/${cmd.id}`).then(full => {
      // The GET response may not include template — resolve it
      api.post<{ prompt: string }>('/api/commands/resolve', { input: `/${cmd.name}`, project_id: null })
        .then(r => setEditForm({ description: full.description, prompt_template: r.prompt, persistent_session: full.persistent_session }))
        .catch(() => setEditForm({ description: full.description, prompt_template: '', persistent_session: full.persistent_session }));
    }).catch(e => console.warn(e));
  };

  const saveEdit = async (e: Event) => {
    e.preventDefault();
    if (!editCmd) return;
    setError('');
    try {
      await api.put(`/api/commands/${editCmd.id}`, {
        description: editForm.description,
        prompt_template: editForm.prompt_template || undefined,
        persistent_session: editForm.persistent_session,
      });
      setEditCmd(null);
      load();
    } catch (err: any) { setError(err.message); }
  };

  const deleteCmd = async () => {
    if (!editCmd) return;
    await api.del(`/api/commands/${editCmd.id}`);
    setEditCmd(null);
    load();
  };

  const previewTemplate = (template: string) =>
    template.replace(/\$ARGUMENTS/g, 'your task here');

  return (
    <div>
      <div class="flex-between mb-md">
        <h2>Global Skills</h2>
        <button class="btn btn-primary" onClick={() => { setShowCreate(true); setError(''); }}>New Skill</button>
      </div>
      <p class="text-muted text-sm mb-md">
        Global skills are available to all projects. Workspace and project-level skills can override these.
      </p>
      <div class="card">
        {commands.length === 0 ? <div class="empty-state">No global skills defined</div> : (
          <table class="table">
            <thead><tr><th>Name</th><th>Description</th><th>Persistent</th><th>Updated</th></tr></thead>
            <tbody>
              {commands.map(c => (
                <tr key={c.id} class="table-link" onClick={() => openEdit(c)}>
                  <td class="mono">/{c.name}</td>
                  <td class="text-sm">{c.description || <span class="text-muted">-</span>}</td>
                  <td>{c.persistent_session && <Badge status="active" />}</td>
                  <td class="text-muted text-sm">{timeAgo(c.updated_at)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
        <Pagination total={total} limit={LIMIT} offset={offset} onChange={setOffset} />
      </div>

      {/* Create Modal */}
      <Modal open={showCreate} onClose={() => setShowCreate(false)} title="New Global Skill" wide>
        <form onSubmit={create}>
          <div class="flex gap-md" style="align-items:flex-start">
            <div style="flex:1">
              <div class="form-group">
                <label>Name</label>
                <input class="input" required placeholder="e.g. dev, plan-review" value={createForm.name}
                  onInput={(e) => setCreateForm({ ...createForm, name: (e.target as HTMLInputElement).value })} />
              </div>
              <div class="form-group">
                <label>Description</label>
                <input class="input" value={createForm.description} placeholder="Brief description of the skill"
                  onInput={(e) => setCreateForm({ ...createForm, description: (e.target as HTMLInputElement).value })} />
              </div>
              <div class="form-group">
                <label>Prompt Template</label>
                <textarea class="input mono" rows={12} required value={createForm.prompt_template}
                  placeholder="Use $ARGUMENTS for user input"
                  onInput={(e) => setCreateForm({ ...createForm, prompt_template: (e.target as HTMLTextAreaElement).value })} />
              </div>
              <div class="form-group">
                <label>
                  <input type="checkbox" checked={createForm.persistent_session}
                    onChange={() => setCreateForm({ ...createForm, persistent_session: !createForm.persistent_session })} />
                  {' '}Persistent session (keep alive after execution)
                </label>
              </div>
            </div>
            {showPreview && createForm.prompt_template && (
              <div style="flex:1;max-height:400px;overflow:auto" class="card">
                <div class="text-xs text-muted mb-sm">Preview (with sample arguments)</div>
                <Markdown content={previewTemplate(createForm.prompt_template)} />
              </div>
            )}
          </div>
          {error && <div class="error-msg">{error}</div>}
          <div class="modal-actions">
            <button type="button" class="btn btn-sm" onClick={() => setShowPreview(!showPreview)}>
              {showPreview ? 'Hide' : 'Show'} Preview
            </button>
            <div style="flex:1" />
            <button type="button" class="btn" onClick={() => setShowCreate(false)}>Cancel</button>
            <button type="submit" class="btn btn-primary">Create</button>
          </div>
        </form>
      </Modal>

      {/* Edit Modal */}
      <Modal open={!!editCmd} onClose={() => setEditCmd(null)} title={`Edit /${editCmd?.name || ''}`} wide>
        <form onSubmit={saveEdit}>
          <div class="form-group">
            <label>Description</label>
            <input class="input" value={editForm.description}
              onInput={(e) => setEditForm({ ...editForm, description: (e.target as HTMLInputElement).value })} />
          </div>
          <div class="form-group">
            <label>Prompt Template</label>
            <textarea class="input mono" rows={12} value={editForm.prompt_template}
              onInput={(e) => setEditForm({ ...editForm, prompt_template: (e.target as HTMLTextAreaElement).value })} />
          </div>
          <div class="form-group">
            <label>
              <input type="checkbox" checked={editForm.persistent_session}
                onChange={() => setEditForm({ ...editForm, persistent_session: !editForm.persistent_session })} />
              {' '}Persistent session
            </label>
          </div>
          {error && <div class="error-msg">{error}</div>}
          <div class="modal-actions">
            <button type="button" class="btn btn-danger" onClick={deleteCmd}>Delete</button>
            <div style="flex:1" />
            <button type="button" class="btn" onClick={() => setEditCmd(null)}>Cancel</button>
            <button type="submit" class="btn btn-primary">Save</button>
          </div>
        </form>
      </Modal>
    </div>
  );
}
