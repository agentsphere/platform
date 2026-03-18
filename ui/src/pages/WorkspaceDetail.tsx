import { useState, useEffect } from 'preact/hooks';
import { api, qs, type ListResponse } from '../lib/api';
import type { Workspace, WorkspaceMember, Project } from '../lib/types';
import { timeAgo } from '../lib/format';
import { Badge } from '../components/Badge';
import { Modal } from '../components/Modal';
import { useAuth } from '../lib/auth';

interface WsCommand {
  id: string;
  project_id: string | null;
  workspace_id: string | null;
  name: string;
  description: string;
  persistent_session: boolean;
  created_at: string;
  updated_at: string;
}

interface Props {
  id?: string;
  path?: string;
}

export function WorkspaceDetail({ id }: Props) {
  const { user } = useAuth();
  const [ws, setWs] = useState<Workspace | null>(null);
  const [members, setMembers] = useState<WorkspaceMember[]>([]);
  const [projects, setProjects] = useState<Project[]>([]);
  const [projectTotal, setProjectTotal] = useState(0);
  const [tab, setTab] = useState<'projects' | 'members' | 'skills' | 'settings'>('projects');
  const [showAddMember, setShowAddMember] = useState(false);
  const [skills, setSkills] = useState<WsCommand[]>([]);
  const [showCreateSkill, setShowCreateSkill] = useState(false);
  const [skillForm, setSkillForm] = useState({ name: '', description: '', prompt_template: '', persistent_session: false });
  const [skillError, setSkillError] = useState('');
  const [addForm, setAddForm] = useState({ user_id: '', role: 'member' });
  const [editForm, setEditForm] = useState({ display_name: '', description: '' });
  const [error, setError] = useState('');

  const loadWorkspace = () => {
    if (!id) return;
    api.get<Workspace>(`/api/workspaces/${id}`).then(w => {
      setWs(w);
      setEditForm({ display_name: w.display_name || '', description: w.description || '' });
    }).catch(() => {});
  };

  const loadMembers = () => {
    if (!id) return;
    api.get<WorkspaceMember[]>(`/api/workspaces/${id}/members`).then(setMembers).catch(() => {});
  };

  const loadProjects = () => {
    if (!id) return;
    api.get<ListResponse<Project>>(`/api/workspaces/${id}/projects${qs({ limit: 50 })}`)
      .then(r => { setProjects(r.items); setProjectTotal(r.total); })
      .catch(() => {});
  };

  const loadSkills = () => {
    if (!id) return;
    api.get<{ items: WsCommand[]; total: number }>(`/api/workspaces/${id}/commands`).then(r => setSkills(r.items)).catch(() => {});
  };

  useEffect(() => { loadWorkspace(); loadMembers(); loadProjects(); loadSkills(); }, [id]);

  const isOwnerOrAdmin = members.some(
    m => m.user_id === user?.id && (m.role === 'owner' || m.role === 'admin')
  );

  const addMember = async (e: Event) => {
    e.preventDefault();
    setError('');
    try {
      await api.post(`/api/workspaces/${id}/members`, {
        user_id: addForm.user_id,
        role: addForm.role,
      });
      setShowAddMember(false);
      setAddForm({ user_id: '', role: 'member' });
      loadMembers();
    } catch (err: any) {
      setError(err.message || 'Failed to add member');
    }
  };

  const removeMember = async (userId: string) => {
    try {
      await api.del(`/api/workspaces/${id}/members/${userId}`);
      loadMembers();
    } catch (err: any) {
      setError(err.message || 'Failed to remove member');
    }
  };

  const updateWorkspace = async (e: Event) => {
    e.preventDefault();
    setError('');
    try {
      const updated = await api.patch<Workspace>(`/api/workspaces/${id}`, {
        display_name: editForm.display_name || undefined,
        description: editForm.description || undefined,
      });
      setWs(updated);
    } catch (err: any) {
      setError(err.message || 'Failed to update workspace');
    }
  };

  const deleteWorkspace = async () => {
    if (!confirm('Delete this workspace? Projects will be unlinked but not deleted.')) return;
    try {
      await api.del(`/api/workspaces/${id}`);
      window.location.href = '/workspaces';
    } catch (err: any) {
      setError(err.message || 'Failed to delete workspace');
    }
  };

  const createSkill = async (e: Event) => {
    e.preventDefault();
    setSkillError('');
    try {
      await api.post(`/api/workspaces/${id}/commands`, skillForm);
      setShowCreateSkill(false);
      setSkillForm({ name: '', description: '', prompt_template: '', persistent_session: false });
      loadSkills();
    } catch (err: any) { setSkillError(err.message); }
  };

  const deleteSkill = async (cmdId: string) => {
    try {
      await api.del(`/api/workspaces/${id}/commands/${cmdId}`);
      loadSkills();
    } catch (err: any) { setError(err.message); }
  };

  if (!ws) return <div class="loading">Loading...</div>;

  return (
    <div>
      <div class="flex-between mb-md">
        <div>
          <h2>{ws.display_name || ws.name}</h2>
          {ws.description && <p class="text-muted">{ws.description}</p>}
        </div>
      </div>

      <div class="tabs mb-md">
        <button class={`tab${tab === 'projects' ? ' active' : ''}`} onClick={() => setTab('projects')}>
          Projects ({projectTotal})
        </button>
        <button class={`tab${tab === 'members' ? ' active' : ''}`} onClick={() => setTab('members')}>
          Members ({members.length})
        </button>
        <button class={`tab${tab === 'skills' ? ' active' : ''}`} onClick={() => setTab('skills')}>
          Skills
        </button>
        {isOwnerOrAdmin && (
          <button class={`tab${tab === 'settings' ? ' active' : ''}`} onClick={() => setTab('settings')}>
            Settings
          </button>
        )}
      </div>

      {error && <div class="error-msg mb-md">{error}</div>}

      {tab === 'projects' && (
        <div class="card">
          {projects.length === 0 ? (
            <div class="empty-state">No projects in this workspace</div>
          ) : (
            <table class="table">
              <thead>
                <tr><th>Name</th><th>Visibility</th><th>Updated</th></tr>
              </thead>
              <tbody>
                {projects.map(p => (
                  <tr key={p.id} class="table-link" onClick={() => { window.location.href = `/projects/${p.id}`; }}>
                    <td><a href={`/projects/${p.id}`}>{p.display_name || p.name}</a></td>
                    <td><Badge status={p.visibility} /></td>
                    <td class="text-muted text-sm">{timeAgo(p.updated_at)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </div>
      )}

      {tab === 'members' && (
        <div class="card">
          {isOwnerOrAdmin && (
            <div class="mb-md">
              <button class="btn btn-primary btn-sm" onClick={() => setShowAddMember(true)}>Add Member</button>
            </div>
          )}
          <table class="table">
            <thead>
              <tr><th>User</th><th>Role</th><th>Added</th>{isOwnerOrAdmin && <th />}</tr>
            </thead>
            <tbody>
              {members.map(m => (
                <tr key={m.id}>
                  <td>{m.user_name}</td>
                  <td><Badge status={m.role} /></td>
                  <td class="text-muted text-sm">{timeAgo(m.created_at)}</td>
                  {isOwnerOrAdmin && (
                    <td>
                      {m.role !== 'owner' && (
                        <button class="btn btn-ghost btn-sm" onClick={() => removeMember(m.user_id)}>Remove</button>
                      )}
                    </td>
                  )}
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}

      {tab === 'skills' && (
        <div class="card">
          {isOwnerOrAdmin && (
            <div class="mb-md">
              <button class="btn btn-primary btn-sm" onClick={() => { setShowCreateSkill(true); setSkillError(''); }}>New Workspace Skill</button>
            </div>
          )}
          {skills.length === 0 ? <div class="empty-state">No skills defined</div> : (
            <table class="table">
              <thead><tr><th>Name</th><th>Scope</th><th>Description</th><th>Persistent</th>{isOwnerOrAdmin && <th />}</tr></thead>
              <tbody>
                {skills.map(c => (
                  <tr key={c.id}>
                    <td class="mono">/{c.name}</td>
                    <td><Badge status={c.workspace_id ? 'workspace' : 'global'} /></td>
                    <td class="text-sm">{c.description || <span class="text-muted">-</span>}</td>
                    <td>{c.persistent_session ? 'Yes' : ''}</td>
                    {isOwnerOrAdmin && (
                      <td>
                        {c.workspace_id && (
                          <button class="btn btn-ghost btn-sm" onClick={() => deleteSkill(c.id)}>Delete</button>
                        )}
                        {!c.workspace_id && (
                          <button class="btn btn-ghost btn-sm" onClick={() => {
                            setSkillForm({ name: c.name, description: c.description, prompt_template: '', persistent_session: c.persistent_session });
                            setShowCreateSkill(true);
                            setSkillError('');
                          }}>Override</button>
                        )}
                      </td>
                    )}
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </div>
      )}

      {tab === 'settings' && isOwnerOrAdmin && (
        <div class="card">
          <form onSubmit={updateWorkspace}>
            <div class="form-group">
              <label>Display Name</label>
              <input class="input" value={editForm.display_name}
                onInput={(e) => setEditForm({ ...editForm, display_name: (e.target as HTMLInputElement).value })} />
            </div>
            <div class="form-group">
              <label>Description</label>
              <textarea class="input" value={editForm.description}
                onInput={(e) => setEditForm({ ...editForm, description: (e.target as HTMLTextAreaElement).value })} />
            </div>
            <button type="submit" class="btn btn-primary">Save</button>
          </form>
          <hr class="my-lg" />
          <div>
            <h3 class="text-danger">Danger Zone</h3>
            <button class="btn btn-danger" onClick={deleteWorkspace}>Delete Workspace</button>
          </div>
        </div>
      )}

      <Modal open={showCreateSkill} onClose={() => setShowCreateSkill(false)} title="New Workspace Skill">
        <form onSubmit={createSkill}>
          <div class="form-group">
            <label>Name</label>
            <input class="input" required placeholder="e.g. dev, plan-review" value={skillForm.name}
              onInput={(e) => setSkillForm({ ...skillForm, name: (e.target as HTMLInputElement).value })} />
          </div>
          <div class="form-group">
            <label>Description</label>
            <input class="input" value={skillForm.description}
              onInput={(e) => setSkillForm({ ...skillForm, description: (e.target as HTMLInputElement).value })} />
          </div>
          <div class="form-group">
            <label>Prompt Template</label>
            <textarea class="input mono" rows={8} required value={skillForm.prompt_template}
              placeholder="Use $ARGUMENTS for user input"
              onInput={(e) => setSkillForm({ ...skillForm, prompt_template: (e.target as HTMLTextAreaElement).value })} />
          </div>
          <div class="form-group">
            <label>
              <input type="checkbox" checked={skillForm.persistent_session}
                onChange={() => setSkillForm({ ...skillForm, persistent_session: !skillForm.persistent_session })} />
              {' '}Persistent session
            </label>
          </div>
          {skillError && <div class="error-msg">{skillError}</div>}
          <div class="modal-actions">
            <button type="button" class="btn" onClick={() => setShowCreateSkill(false)}>Cancel</button>
            <button type="submit" class="btn btn-primary">Create</button>
          </div>
        </form>
      </Modal>

      <Modal open={showAddMember} onClose={() => setShowAddMember(false)} title="Add Member">
        <form onSubmit={addMember}>
          <div class="form-group">
            <label>User ID</label>
            <input class="input" required placeholder="UUID" value={addForm.user_id}
              onInput={(e) => setAddForm({ ...addForm, user_id: (e.target as HTMLInputElement).value })} />
          </div>
          <div class="form-group">
            <label>Role</label>
            <select class="input" value={addForm.role}
              onChange={(e) => setAddForm({ ...addForm, role: (e.target as HTMLSelectElement).value })}>
              <option value="member">Member</option>
              <option value="admin">Admin</option>
            </select>
          </div>
          {error && <div class="error-msg">{error}</div>}
          <div class="modal-actions">
            <button type="button" class="btn" onClick={() => setShowAddMember(false)}>Cancel</button>
            <button type="submit" class="btn btn-primary">Add</button>
          </div>
        </form>
      </Modal>
    </div>
  );
}
