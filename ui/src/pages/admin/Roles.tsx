import { useState, useEffect } from 'preact/hooks';
import { api } from '../../lib/api';
import type { Role, Permission } from '../../lib/types';
import { Badge } from '../../components/Badge';
import { Modal } from '../../components/Modal';

export function Roles() {
  const [roles, setRoles] = useState<Role[]>([]);
  const [selectedRole, setSelectedRole] = useState<Role | null>(null);
  const [allPerms, setAllPerms] = useState<Permission[]>([]);
  const [rolePerms, setRolePerms] = useState<string[]>([]);
  const [showCreate, setShowCreate] = useState(false);
  const [form, setForm] = useState({ name: '', description: '' });
  const [error, setError] = useState('');

  const load = () => {
    api.get<Role[]>('/api/admin/roles').then(setRoles).catch(() => {});
  };
  useEffect(load, []);

  const selectRole = async (role: Role) => {
    setSelectedRole(role);
    setError('');
    try {
      const perms = await api.get<Permission[]>(`/api/admin/roles/${role.id}/permissions`);
      setRolePerms(perms.map(p => p.name));
      if (allPerms.length === 0) {
        // Fetch all permissions from the first system role (admin) to get the full list
        const adminRole = roles.find(r => r.name === 'admin');
        if (adminRole) {
          const all = await api.get<Permission[]>(`/api/admin/roles/${adminRole.id}/permissions`);
          setAllPerms(all);
        }
      }
    } catch (err: any) { setError(err.message); }
  };

  const togglePerm = (permName: string) => {
    setRolePerms(prev =>
      prev.includes(permName) ? prev.filter(p => p !== permName) : [...prev, permName]
    );
  };

  const savePerms = async () => {
    if (!selectedRole) return;
    try {
      await api.put(`/api/admin/roles/${selectedRole.id}/permissions`, { permissions: rolePerms });
      setError('');
    } catch (err: any) { setError(err.message); }
  };

  const create = async (e: Event) => {
    e.preventDefault();
    setError('');
    try {
      await api.post('/api/admin/roles', form);
      setShowCreate(false);
      setForm({ name: '', description: '' });
      load();
    } catch (err: any) { setError(err.message); }
  };

  return (
    <div>
      <div class="flex-between mb-md">
        <h2>Roles</h2>
        <button class="btn btn-primary" onClick={() => { setShowCreate(true); setError(''); }}>New Role</button>
      </div>

      <div class="flex gap-md">
        <div class="card" style="min-width:240px">
          {roles.map(r => (
            <div key={r.id}
              class={`tree-entry${selectedRole?.id === r.id ? ' active' : ''}`}
              style={selectedRole?.id === r.id ? 'background:var(--bg-hover)' : ''}
              onClick={() => selectRole(r)}>
              <span>{r.name}</span>
              {r.is_system && <Badge status="active" />}
            </div>
          ))}
        </div>

        {selectedRole && (
          <div class="card" style="flex:1">
            <div class="flex-between mb-md">
              <div>
                <h3 style="font-size:1rem">{selectedRole.name}</h3>
                {selectedRole.description && <p class="text-sm text-muted">{selectedRole.description}</p>}
              </div>
              {!selectedRole.is_system && (
                <button class="btn btn-primary btn-sm" onClick={savePerms}>Save Permissions</button>
              )}
            </div>
            {allPerms.length === 0 ? (
              <div class="text-muted text-sm">Select a role to view permissions</div>
            ) : (
              <div>
                {allPerms.map(p => (
                  <label key={p.name} class="flex gap-sm" style="padding:0.3rem 0;cursor:pointer">
                    <input type="checkbox" checked={rolePerms.includes(p.name)}
                      onChange={() => togglePerm(p.name)}
                      disabled={selectedRole.is_system} />
                    <span class="text-sm">{p.name}</span>
                    {p.description && <span class="text-xs text-muted">— {p.description}</span>}
                  </label>
                ))}
              </div>
            )}
            {error && <div class="error-msg mt-sm">{error}</div>}
          </div>
        )}
      </div>

      <Modal open={showCreate} onClose={() => setShowCreate(false)} title="New Role">
        <form onSubmit={create}>
          <div class="form-group">
            <label>Name</label>
            <input class="input" required value={form.name}
              onInput={(e) => setForm({ ...form, name: (e.target as HTMLInputElement).value })} />
          </div>
          <div class="form-group">
            <label>Description</label>
            <input class="input" value={form.description}
              onInput={(e) => setForm({ ...form, description: (e.target as HTMLInputElement).value })} />
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
