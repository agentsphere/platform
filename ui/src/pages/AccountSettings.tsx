import { useState, useEffect } from 'preact/hooks';
import { useAuth } from '../lib/auth';
import { api, ApiError } from '../lib/api';
import { timeAgo } from '../lib/format';
import { Modal } from '../components/Modal';
import { prepareCreationOptions, serializeRegistrationResponse } from '../lib/webauthn';
import type { PasskeyResponse } from '../lib/types';

export function AccountSettings() {
  return (
    <div>
      <h2 style="margin-bottom:1rem">Account Settings</h2>
      <ChangePassword />
      {window.PublicKeyCredential && <PasskeySection />}
    </div>
  );
}

function ChangePassword() {
  const { user } = useAuth();
  const [currentPw, setCurrentPw] = useState('');
  const [newPw, setNewPw] = useState('');
  const [confirmPw, setConfirmPw] = useState('');
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState('');
  const [success, setSuccess] = useState('');

  const submit = async (e: Event) => {
    e.preventDefault();
    setError('');
    setSuccess('');
    if (newPw.length < 8) { setError('Password must be at least 8 characters'); return; }
    if (newPw !== confirmPw) { setError('Passwords do not match'); return; }
    setSaving(true);
    try {
      await api.patch(`/api/users/${user!.id}`, { password: newPw, current_password: currentPw });
      setCurrentPw('');
      setNewPw('');
      setConfirmPw('');
      setSuccess('Password changed successfully');
    } catch (err) {
      setError(err instanceof ApiError ? err.body.error : 'Failed to change password');
    } finally {
      setSaving(false);
    }
  };

  return (
    <div class="card" style="margin-bottom:1rem">
      <div class="card-header">
        <span class="card-title">Change Password</span>
      </div>
      <div style="padding:1rem">
        <form onSubmit={submit}>
          <div class="form-group">
            <label>Current Password</label>
            <input class="input" type="password" value={currentPw}
              onInput={(e) => setCurrentPw((e.target as HTMLInputElement).value)} />
          </div>
          <div class="form-group">
            <label>New Password</label>
            <input class="input" type="password" value={newPw}
              onInput={(e) => setNewPw((e.target as HTMLInputElement).value)}
              minLength={8} />
          </div>
          <div class="form-group">
            <label>Confirm New Password</label>
            <input class="input" type="password" value={confirmPw}
              onInput={(e) => setConfirmPw((e.target as HTMLInputElement).value)}
              minLength={8} />
          </div>
          {error && <div class="error-msg">{error}</div>}
          {success && <div class="success-msg">{success}</div>}
          <button type="submit" class="btn btn-primary btn-sm" disabled={saving || !newPw}>
            {saving ? 'Changing...' : 'Change Password'}
          </button>
        </form>
      </div>
    </div>
  );
}

function PasskeySection() {
  const [passkeys, setPasskeys] = useState<PasskeyResponse[]>([]);
  const [loading, setLoading] = useState(true);
  const [registerOpen, setRegisterOpen] = useState(false);
  const [registerName, setRegisterName] = useState('');
  const [registering, setRegistering] = useState(false);
  const [renameId, setRenameId] = useState<string | null>(null);
  const [renameName, setRenameName] = useState('');
  const [error, setError] = useState('');
  const [success, setSuccess] = useState('');

  const load = () => {
    api.get<PasskeyResponse[]>('/api/auth/passkeys')
      .then(setPasskeys)
      .catch(e => console.warn(e))
      .finally(() => setLoading(false));
  };

  useEffect(() => { load(); }, []);

  const register = async () => {
    if (!registerName.trim()) return;
    setRegistering(true);
    setError('');
    try {
      const challenge = await api.post<any>('/api/auth/passkeys/register/begin', { name: registerName });
      const opts = prepareCreationOptions(challenge);
      const cred = await navigator.credentials.create({ publicKey: opts }) as PublicKeyCredential;
      if (!cred) throw new Error('Registration was cancelled');
      const serialized = serializeRegistrationResponse(cred);
      await api.post('/api/auth/passkeys/register/complete', serialized);
      setRegisterOpen(false);
      setRegisterName('');
      setSuccess('Passkey registered');
      load();
    } catch (err) {
      setError(err instanceof ApiError ? err.body.error : (err as Error).message);
    } finally {
      setRegistering(false);
    }
  };

  const rename = async () => {
    if (!renameId || !renameName.trim()) return;
    setError('');
    try {
      await api.patch(`/api/auth/passkeys/${renameId}`, { name: renameName });
      setRenameId(null);
      setRenameName('');
      setSuccess('Passkey renamed');
      load();
    } catch (err) {
      setError(err instanceof ApiError ? err.body.error : 'Rename failed');
    }
  };

  const remove = async (id: string) => {
    if (!confirm('Delete this passkey?')) return;
    setError('');
    try {
      await api.del(`/api/auth/passkeys/${id}`);
      setSuccess('Passkey deleted');
      load();
    } catch (err) {
      setError(err instanceof ApiError ? err.body.error : 'Delete failed');
    }
  };

  return (
    <div class="card">
      <div class="card-header">
        <span class="card-title">Passkeys</span>
        <button class="btn btn-primary btn-sm" onClick={() => { setRegisterOpen(true); setError(''); setSuccess(''); }}>
          Register New Passkey
        </button>
      </div>
      <div style="padding:1rem">
        {error && <div class="error-msg">{error}</div>}
        {success && <div class="success-msg">{success}</div>}

        {loading ? (
          <div class="text-muted text-sm">Loading...</div>
        ) : passkeys.length === 0 ? (
          <div class="text-muted text-sm">No passkeys registered</div>
        ) : (
          <table class="table">
            <thead>
              <tr>
                <th>Name</th>
                <th>Created</th>
                <th>Last Used</th>
                <th>Actions</th>
              </tr>
            </thead>
            <tbody>
              {passkeys.map(pk => (
                <tr key={pk.id}>
                  <td>{pk.name}</td>
                  <td>{timeAgo(pk.created_at)}</td>
                  <td>{pk.last_used_at ? timeAgo(pk.last_used_at) : 'Never'}</td>
                  <td>
                    <button class="btn btn-ghost btn-sm" onClick={() => { setRenameId(pk.id); setRenameName(pk.name); setError(''); setSuccess(''); }}>
                      Rename
                    </button>
                    <button class="btn btn-danger btn-sm" style="margin-left:0.25rem" onClick={() => remove(pk.id)}>
                      Delete
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>

      <Modal open={registerOpen} onClose={() => setRegisterOpen(false)} title="Register New Passkey">
        <div class="form-group">
          <label>Passkey Name</label>
          <input class="input" type="text" value={registerName} placeholder="e.g. MacBook Touch ID"
            onInput={(e) => setRegisterName((e.target as HTMLInputElement).value)}
            autoFocus />
        </div>
        <div style="display:flex;gap:0.5rem;justify-content:flex-end;margin-top:1rem">
          <button class="btn btn-ghost btn-sm" onClick={() => setRegisterOpen(false)}>Cancel</button>
          <button class="btn btn-primary btn-sm" disabled={registering || !registerName.trim()} onClick={register}>
            {registering ? 'Registering...' : 'Register'}
          </button>
        </div>
      </Modal>

      <Modal open={!!renameId} onClose={() => setRenameId(null)} title="Rename Passkey">
        <div class="form-group">
          <label>New Name</label>
          <input class="input" type="text" value={renameName}
            onInput={(e) => setRenameName((e.target as HTMLInputElement).value)}
            autoFocus />
        </div>
        <div style="display:flex;gap:0.5rem;justify-content:flex-end;margin-top:1rem">
          <button class="btn btn-ghost btn-sm" onClick={() => setRenameId(null)}>Cancel</button>
          <button class="btn btn-primary btn-sm" disabled={!renameName.trim()} onClick={rename}>
            Save
          </button>
        </div>
      </Modal>
    </div>
  );
}
