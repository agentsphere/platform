import { useState, useEffect } from 'preact/hooks';
import { api, qs, type ListResponse } from '../../lib/api';
import type { User } from '../../lib/types';
import { timeAgo } from '../../lib/format';
import { Badge } from '../../components/Badge';
import { Pagination } from '../../components/Pagination';
import { Modal } from '../../components/Modal';

const LIMIT = 20;

export function Users() {
  const [users, setUsers] = useState<User[]>([]);
  const [total, setTotal] = useState(0);
  const [offset, setOffset] = useState(0);
  const [showCreate, setShowCreate] = useState(false);
  const [editUser, setEditUser] = useState<User | null>(null);
  const [createForm, setCreateForm] = useState({ name: '', email: '', password: '', display_name: '' });
  const [editForm, setEditForm] = useState({ display_name: '', email: '', password: '' });
  const [error, setError] = useState('');

  const load = () => {
    api.get<ListResponse<User>>(`/api/users/list${qs({ limit: LIMIT, offset })}`)
      .then(r => { setUsers(r.items); setTotal(r.total); }).catch(() => {});
  };
  useEffect(load, [offset]);

  const create = async (e: Event) => {
    e.preventDefault();
    setError('');
    try {
      await api.post('/api/users', {
        ...createForm,
        display_name: createForm.display_name || undefined,
      });
      setShowCreate(false);
      setCreateForm({ name: '', email: '', password: '', display_name: '' });
      load();
    } catch (err: any) { setError(err.message); }
  };

  const openEdit = (u: User) => {
    setEditUser(u);
    setEditForm({ display_name: u.display_name || '', email: u.email, password: '' });
    setError('');
  };

  const saveEdit = async (e: Event) => {
    e.preventDefault();
    if (!editUser) return;
    setError('');
    try {
      const body: Record<string, string> = {};
      if (editForm.display_name !== (editUser.display_name || '')) body.display_name = editForm.display_name;
      if (editForm.email !== editUser.email) body.email = editForm.email;
      if (editForm.password) body.password = editForm.password;
      await api.patch(`/api/users/${editUser.id}`, body);
      setEditUser(null);
      load();
    } catch (err: any) { setError(err.message); }
  };

  const deactivate = async () => {
    if (!editUser) return;
    await api.del(`/api/users/${editUser.id}`);
    setEditUser(null);
    load();
  };

  return (
    <div>
      <div class="flex-between mb-md">
        <h2>Users</h2>
        <button class="btn btn-primary" onClick={() => { setShowCreate(true); setError(''); }}>New User</button>
      </div>
      <div class="card">
        {users.length === 0 ? <div class="empty-state">No users</div> : (
          <table class="table">
            <thead><tr><th>Name</th><th>Email</th><th>Status</th><th>Created</th></tr></thead>
            <tbody>
              {users.map(u => (
                <tr key={u.id} class="table-link" onClick={() => openEdit(u)}>
                  <td>{u.display_name || u.name}</td>
                  <td class="text-sm">{u.email}</td>
                  <td><Badge status={u.is_active ? 'active' : 'inactive'} /></td>
                  <td class="text-muted text-sm">{timeAgo(u.created_at)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
        <Pagination total={total} limit={LIMIT} offset={offset} onChange={setOffset} />
      </div>

      <Modal open={showCreate} onClose={() => setShowCreate(false)} title="New User">
        <form onSubmit={create}>
          <div class="form-group">
            <label>Username</label>
            <input class="input" required value={createForm.name}
              onInput={(e) => setCreateForm({ ...createForm, name: (e.target as HTMLInputElement).value })} />
          </div>
          <div class="form-group">
            <label>Email</label>
            <input class="input" type="email" required value={createForm.email}
              onInput={(e) => setCreateForm({ ...createForm, email: (e.target as HTMLInputElement).value })} />
          </div>
          <div class="form-group">
            <label>Password</label>
            <input class="input" type="password" required value={createForm.password}
              onInput={(e) => setCreateForm({ ...createForm, password: (e.target as HTMLInputElement).value })} />
          </div>
          <div class="form-group">
            <label>Display Name (optional)</label>
            <input class="input" value={createForm.display_name}
              onInput={(e) => setCreateForm({ ...createForm, display_name: (e.target as HTMLInputElement).value })} />
          </div>
          {error && <div class="error-msg">{error}</div>}
          <div class="modal-actions">
            <button type="button" class="btn" onClick={() => setShowCreate(false)}>Cancel</button>
            <button type="submit" class="btn btn-primary">Create</button>
          </div>
        </form>
      </Modal>

      <Modal open={!!editUser} onClose={() => setEditUser(null)} title={`Edit ${editUser?.name || ''}`}>
        <form onSubmit={saveEdit}>
          <div class="form-group">
            <label>Display Name</label>
            <input class="input" value={editForm.display_name}
              onInput={(e) => setEditForm({ ...editForm, display_name: (e.target as HTMLInputElement).value })} />
          </div>
          <div class="form-group">
            <label>Email</label>
            <input class="input" type="email" value={editForm.email}
              onInput={(e) => setEditForm({ ...editForm, email: (e.target as HTMLInputElement).value })} />
          </div>
          <div class="form-group">
            <label>New Password (leave blank to keep)</label>
            <input class="input" type="password" value={editForm.password}
              onInput={(e) => setEditForm({ ...editForm, password: (e.target as HTMLInputElement).value })} />
          </div>
          {error && <div class="error-msg">{error}</div>}
          <div class="modal-actions">
            {editUser?.is_active && (
              <button type="button" class="btn btn-danger" onClick={deactivate}>Deactivate</button>
            )}
            <div style="flex:1" />
            <button type="button" class="btn" onClick={() => setEditUser(null)}>Cancel</button>
            <button type="submit" class="btn btn-primary">Save</button>
          </div>
        </form>
      </Modal>
    </div>
  );
}
