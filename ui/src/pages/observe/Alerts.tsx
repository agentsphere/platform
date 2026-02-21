import { useState, useEffect } from 'preact/hooks';
import { api, type ListResponse } from '../../lib/api';
import type { AlertRule, AlertEvent } from '../../lib/types';
import { timeAgo } from '../../lib/format';
import { Badge } from '../../components/Badge';
import { Modal } from '../../components/Modal';
import { StatusDot } from '../../components/StatusDot';

export function Alerts() {
  const [rules, setRules] = useState<AlertRule[]>([]);
  const [events, setEvents] = useState<AlertEvent[]>([]);
  const [showCreate, setShowCreate] = useState(false);
  const [editingRule, setEditingRule] = useState<AlertRule | null>(null);
  const [form, setForm] = useState({
    name: '', query: '', condition: 'gt', threshold: '0',
    window_seconds: '300', channels: '', enabled: true,
  });
  const [error, setError] = useState('');

  const loadRules = () => {
    api.get<ListResponse<AlertRule>>('/api/observe/alerts?limit=100')
      .then(r => setRules(r.items)).catch(() => {});
  };

  const loadEvents = () => {
    api.get<ListResponse<AlertEvent>>('/api/observe/alerts/events?limit=50')
      .then(r => setEvents(r.items)).catch(() => {});
  };

  useEffect(() => { loadRules(); loadEvents(); }, []);

  const resetForm = () => {
    setForm({ name: '', query: '', condition: 'gt', threshold: '0', window_seconds: '300', channels: '', enabled: true });
    setError('');
    setEditingRule(null);
  };

  const openEdit = (rule: AlertRule) => {
    setEditingRule(rule);
    setForm({
      name: rule.name,
      query: rule.query,
      condition: rule.condition,
      threshold: String(rule.threshold),
      window_seconds: String(rule.window_seconds),
      channels: rule.channels.join(', '),
      enabled: rule.enabled,
    });
    setShowCreate(true);
  };

  const save = async (e: Event) => {
    e.preventDefault();
    setError('');
    const body = {
      name: form.name,
      query: form.query,
      condition: form.condition,
      threshold: parseFloat(form.threshold),
      window_seconds: parseInt(form.window_seconds),
      channels: form.channels.split(',').map(s => s.trim()).filter(Boolean),
      enabled: form.enabled,
    };

    try {
      if (editingRule) {
        await api.put(`/api/observe/alerts/${editingRule.id}`, body);
      } else {
        await api.post('/api/observe/alerts', body);
      }
      setShowCreate(false);
      resetForm();
      loadRules();
    } catch (err: any) { setError(err.message); }
  };

  const toggleEnabled = async (rule: AlertRule) => {
    try {
      await api.patch(`/api/observe/alerts/${rule.id}`, { enabled: !rule.enabled });
      loadRules();
    } catch { /* ignore */ }
  };

  const deleteRule = async (id: string) => {
    if (!confirm('Delete this alert rule?')) return;
    await api.del(`/api/observe/alerts/${id}`);
    loadRules();
  };

  return (
    <div>
      <div class="flex-between mb-md">
        <h2>Alerts</h2>
        <button class="btn btn-primary" onClick={() => { resetForm(); setShowCreate(true); }}>
          New Alert Rule
        </button>
      </div>

      <div class="card mb-md">
        <div class="card-header">
          <span class="card-title">Alert Rules</span>
        </div>
        {rules.length === 0 ? (
          <div class="empty-state">No alert rules configured</div>
        ) : (
          <table class="table">
            <thead>
              <tr>
                <th>Status</th>
                <th>Name</th>
                <th>Condition</th>
                <th>Window</th>
                <th>Channels</th>
                <th>Actions</th>
              </tr>
            </thead>
            <tbody>
              {rules.map(rule => (
                <tr key={rule.id}>
                  <td>
                    <StatusDot status={rule.enabled ? 'active' : 'inactive'}
                      label={rule.enabled ? 'Enabled' : 'Disabled'} />
                  </td>
                  <td>{rule.name}</td>
                  <td class="mono text-xs">
                    {rule.query} {rule.condition} {rule.threshold}
                  </td>
                  <td class="text-sm">{rule.window_seconds}s</td>
                  <td class="text-xs">{rule.channels.join(', ')}</td>
                  <td>
                    <div class="flex gap-sm">
                      <button class="btn btn-ghost btn-sm"
                        onClick={() => toggleEnabled(rule)}>
                        {rule.enabled ? 'Disable' : 'Enable'}
                      </button>
                      <button class="btn btn-ghost btn-sm" onClick={() => openEdit(rule)}>Edit</button>
                      <button class="btn btn-danger btn-sm" onClick={() => deleteRule(rule.id)}>Delete</button>
                    </div>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>

      <div class="card">
        <div class="card-header">
          <span class="card-title">Alert History</span>
        </div>
        {events.length === 0 ? (
          <div class="empty-state">No alert events</div>
        ) : (
          <table class="table">
            <thead>
              <tr>
                <th>Status</th>
                <th>Message</th>
                <th>Value</th>
                <th>Time</th>
              </tr>
            </thead>
            <tbody>
              {events.map(ev => (
                <tr key={ev.id}>
                  <td><Badge status={ev.status} /></td>
                  <td class="text-sm">{ev.message}</td>
                  <td class="mono text-sm">{ev.value}</td>
                  <td class="text-muted text-sm">{timeAgo(ev.created_at)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>

      <Modal open={showCreate} onClose={() => { setShowCreate(false); resetForm(); }}
        title={editingRule ? 'Edit Alert Rule' : 'New Alert Rule'}>
        <form onSubmit={save}>
          <div class="form-group">
            <label>Name</label>
            <input class="input" required value={form.name}
              onInput={(e) => setForm({ ...form, name: (e.target as HTMLInputElement).value })} />
          </div>
          <div class="form-group">
            <label>Query (metric name)</label>
            <input class="input" required value={form.query}
              onInput={(e) => setForm({ ...form, query: (e.target as HTMLInputElement).value })} />
          </div>
          <div class="flex gap-sm">
            <div class="form-group" style="flex:1">
              <label>Condition</label>
              <select class="input" value={form.condition}
                onChange={(e) => setForm({ ...form, condition: (e.target as HTMLSelectElement).value })}>
                <option value="gt">Greater than</option>
                <option value="lt">Less than</option>
                <option value="eq">Equals</option>
                <option value="gte">Greater or equal</option>
                <option value="lte">Less or equal</option>
              </select>
            </div>
            <div class="form-group" style="flex:1">
              <label>Threshold</label>
              <input class="input" type="number" step="any" required value={form.threshold}
                onInput={(e) => setForm({ ...form, threshold: (e.target as HTMLInputElement).value })} />
            </div>
          </div>
          <div class="form-group">
            <label>Window (seconds)</label>
            <input class="input" type="number" min="60" required value={form.window_seconds}
              onInput={(e) => setForm({ ...form, window_seconds: (e.target as HTMLInputElement).value })} />
          </div>
          <div class="form-group">
            <label>Notification channels (comma-separated)</label>
            <input class="input" value={form.channels}
              placeholder="email, webhook"
              onInput={(e) => setForm({ ...form, channels: (e.target as HTMLInputElement).value })} />
          </div>
          <div class="form-group">
            <label class="flex gap-sm" style="align-items:center;cursor:pointer">
              <input type="checkbox" checked={form.enabled}
                onChange={(e) => setForm({ ...form, enabled: (e.target as HTMLInputElement).checked })} />
              <span>Enabled</span>
            </label>
          </div>
          {error && <div class="error-msg">{error}</div>}
          <div class="modal-actions">
            <button type="button" class="btn" onClick={() => { setShowCreate(false); resetForm(); }}>Cancel</button>
            <button type="submit" class="btn btn-primary">{editingRule ? 'Save' : 'Create'}</button>
          </div>
        </form>
      </Modal>
    </div>
  );
}
