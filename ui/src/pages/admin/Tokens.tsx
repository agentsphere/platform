import { useState, useEffect } from 'preact/hooks';
import { api, type ListResponse } from '../../lib/api';
import type { ApiToken, CreateTokenResponse } from '../../lib/types';
import { timeAgo } from '../../lib/format';
import { Modal } from '../../components/Modal';

export function Tokens() {
  const [tokens, setTokens] = useState<ApiToken[]>([]);
  const [showCreate, setShowCreate] = useState(false);
  const [newToken, setNewToken] = useState<string | null>(null);
  const [form, setForm] = useState({ name: '', scopes: '', expires_in_days: '90' });
  const [error, setError] = useState('');

  const load = () => {
    api.get<ListResponse<ApiToken>>('/api/tokens?limit=100')
      .then(r => setTokens(r.items)).catch(() => {});
  };
  useEffect(load, []);

  const create = async (e: Event) => {
    e.preventDefault();
    setError('');
    try {
      const scopes = form.scopes ? form.scopes.split(',').map(s => s.trim()).filter(Boolean) : undefined;
      const res = await api.post<CreateTokenResponse>('/api/tokens', {
        name: form.name,
        scopes,
        expires_in_days: parseInt(form.expires_in_days) || 90,
      });
      setNewToken(res.token);
      setForm({ name: '', scopes: '', expires_in_days: '90' });
      load();
    } catch (err: any) { setError(err.message); }
  };

  const revoke = async (id: string) => {
    await api.del(`/api/tokens/${id}`);
    load();
  };

  const copyToken = () => {
    if (newToken) navigator.clipboard.writeText(newToken);
  };

  return (
    <div>
      <div class="flex-between mb-md">
        <h2>API Tokens</h2>
        <button class="btn btn-primary" onClick={() => { setShowCreate(true); setNewToken(null); setError(''); }}>
          New Token
        </button>
      </div>
      <div class="card">
        {tokens.length === 0 ? <div class="empty-state">No tokens</div> : (
          <table class="table">
            <thead><tr><th>Name</th><th>Scopes</th><th>Last Used</th><th>Expires</th><th></th></tr></thead>
            <tbody>
              {tokens.map(t => (
                <tr key={t.id}>
                  <td>{t.name}</td>
                  <td class="text-xs">{t.scopes.length > 0 ? t.scopes.join(', ') : 'all'}</td>
                  <td class="text-sm text-muted">{t.last_used_at ? timeAgo(t.last_used_at) : 'never'}</td>
                  <td class="text-sm text-muted">{t.expires_at ? timeAgo(t.expires_at) : 'never'}</td>
                  <td><button class="btn btn-danger btn-sm" onClick={() => revoke(t.id)}>Revoke</button></td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>

      <Modal open={showCreate} onClose={() => setShowCreate(false)} title="New API Token">
        {newToken ? (
          <div>
            <div class="mb-md text-sm" style="color:var(--warning)">
              Copy this token now. It will not be shown again.
            </div>
            <div class="flex gap-sm">
              <input class="input mono" readOnly value={newToken} style="font-size:12px" />
              <button class="btn btn-sm" onClick={copyToken}>Copy</button>
            </div>
            <div class="modal-actions">
              <button class="btn btn-primary" onClick={() => { setShowCreate(false); setNewToken(null); }}>Done</button>
            </div>
          </div>
        ) : (
          <form onSubmit={create}>
            <div class="form-group">
              <label>Name</label>
              <input class="input" required value={form.name}
                onInput={(e) => setForm({ ...form, name: (e.target as HTMLInputElement).value })} />
            </div>
            <div class="form-group">
              <label>Scopes (comma-separated, leave empty for all)</label>
              <input class="input" value={form.scopes}
                onInput={(e) => setForm({ ...form, scopes: (e.target as HTMLInputElement).value })} />
            </div>
            <div class="form-group">
              <label>Expires in (days)</label>
              <input class="input" type="number" min="1" max="365" value={form.expires_in_days}
                onInput={(e) => setForm({ ...form, expires_in_days: (e.target as HTMLInputElement).value })} />
            </div>
            {error && <div class="error-msg">{error}</div>}
            <div class="modal-actions">
              <button type="button" class="btn" onClick={() => setShowCreate(false)}>Cancel</button>
              <button type="submit" class="btn btn-primary">Create</button>
            </div>
          </form>
        )}
      </Modal>
    </div>
  );
}
