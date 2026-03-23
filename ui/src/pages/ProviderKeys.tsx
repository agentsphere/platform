import { useState, useEffect } from 'preact/hooks';
import { api } from '../lib/api';
import { timeAgo } from '../lib/format';

interface ProviderKeyMeta {
  provider: string;
  key_suffix: string;
  created_at: string;
  updated_at: string;
}

interface CredentialStatus {
  exists: boolean;
  auth_type?: string;
  token_expires_at?: string;
  created_at?: string;
  updated_at?: string;
}

interface LlmProviderConfig {
  id: string;
  provider_type: string;
  label: string;
  model: string | null;
  validation_status: string;
  last_validated_at: string | null;
  created_at: string;
  updated_at: string;
}

interface ActiveProviderInfo {
  provider: string;
  provider_type: string | null;
  label: string | null;
  has_oauth: boolean;
  has_api_key: boolean;
  custom_configs: LlmProviderConfig[];
}

interface ValidationTestResult {
  test: number;
  name: string;
  status: string;
  detail: string;
}

type CustomProviderType = 'bedrock' | 'vertex' | 'azure_foundry' | 'custom_endpoint';

const PROVIDER_LABELS: Record<string, string> = {
  bedrock: 'AWS Bedrock',
  vertex: 'Vertex AI',
  azure_foundry: 'Azure Foundry',
  custom_endpoint: 'Custom Endpoint',
};

const STATUS_BADGES: Record<string, { class: string; label: string }> = {
  valid: { class: 'badge-success', label: 'Valid' },
  invalid: { class: 'badge-danger', label: 'Invalid' },
  untested: { class: 'badge-warning', label: 'Untested' },
};

export function ProviderKeys() {
  const [keys, setKeys] = useState<ProviderKeyMeta[]>([]);
  const [apiKey, setApiKey] = useState('');
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState('');
  const [success, setSuccess] = useState('');

  // CLI OAuth state
  const [cliCreds, setCliCreds] = useState<CredentialStatus>({ exists: false });
  const [oauthToken, setOauthToken] = useState('');
  const [oauthSaving, setOauthSaving] = useState(false);
  const [oauthError, setOauthError] = useState('');
  const [oauthSuccess, setOauthSuccess] = useState('');

  // Active provider + custom configs
  const [activeInfo, setActiveInfo] = useState<ActiveProviderInfo | null>(null);
  const [switching, setSwitching] = useState(false);

  // Custom provider add form
  const [showAddForm, setShowAddForm] = useState(false);
  const [addType, setAddType] = useState<CustomProviderType | null>(null);
  const [addEnvVars, setAddEnvVars] = useState<Record<string, string>>({});
  const [addModel, setAddModel] = useState('');
  const [addLabel, setAddLabel] = useState('');
  const [addSaving, setAddSaving] = useState(false);
  const [addError, setAddError] = useState('');

  // Validation state
  const [validatingId, setValidatingId] = useState<string | null>(null);
  const [validationResults, setValidationResults] = useState<ValidationTestResult[]>([]);
  const [validationDone, setValidationDone] = useState(false);

  const load = () => {
    api.get<ProviderKeyMeta[]>('/api/users/me/provider-keys')
      .then(setKeys).catch(() => {});
  };

  const loadCliCreds = () => {
    api.get<CredentialStatus>('/api/auth/cli-credentials')
      .then(setCliCreds).catch(() => {});
  };

  const loadActive = () => {
    api.get<ActiveProviderInfo>('/api/users/me/active-provider')
      .then(setActiveInfo).catch(() => {});
  };

  useEffect(() => { load(); loadCliCreds(); loadActive(); }, []);

  const existing = keys.find(k => k.provider === 'anthropic');

  const save = async (e: Event) => {
    e.preventDefault();
    setError('');
    setSuccess('');
    setSaving(true);
    try {
      await api.put('/api/users/me/provider-keys/anthropic', { api_key: apiKey });
      setApiKey('');
      setSuccess('API key saved');
      load();
      loadActive();
    } catch (err: any) {
      setError(err.message);
    } finally {
      setSaving(false);
    }
  };

  const remove = async () => {
    setError('');
    setSuccess('');
    try {
      await api.del('/api/users/me/provider-keys/anthropic');
      setSuccess('API key removed');
      load();
      loadActive();
    } catch (err: any) {
      setError(err.message);
    }
  };

  const saveOauth = async (e: Event) => {
    e.preventDefault();
    setOauthError('');
    setOauthSuccess('');
    setOauthSaving(true);
    try {
      await api.post('/api/auth/cli-credentials', { auth_type: 'oauth', token: oauthToken });
      setOauthToken('');
      setOauthSuccess('OAuth token saved');
      loadCliCreds();
      loadActive();
    } catch (err: any) {
      setOauthError(err.message);
    } finally {
      setOauthSaving(false);
    }
  };

  const removeOauth = async () => {
    setOauthError('');
    setOauthSuccess('');
    try {
      await api.del('/api/auth/cli-credentials');
      setOauthSuccess('OAuth token removed');
      loadCliCreds();
      loadActive();
    } catch (err: any) {
      setOauthError(err.message);
    }
  };

  const switchProvider = async (value: string) => {
    setSwitching(true);
    try {
      await api.put('/api/users/me/active-provider', { provider: value });
      loadActive();
    } catch (err: any) {
      setError(err.message);
    } finally {
      setSwitching(false);
    }
  };

  const addCustomProvider = async () => {
    if (!addType) return;
    setAddSaving(true);
    setAddError('');
    try {
      // Filter out empty values
      const envVars: Record<string, string> = {};
      for (const [k, v] of Object.entries(addEnvVars)) {
        if (v.trim()) envVars[k] = v.trim();
      }
      await api.post('/api/users/me/llm-providers', {
        provider_type: addType,
        label: addLabel || undefined,
        env_vars: envVars,
        model: addModel || undefined,
      });
      setShowAddForm(false);
      setAddType(null);
      setAddEnvVars({});
      setAddModel('');
      setAddLabel('');
      loadActive();
    } catch (err: any) {
      setAddError(err.message);
    } finally {
      setAddSaving(false);
    }
  };

  const deleteCustomProvider = async (id: string) => {
    try {
      await api.del(`/api/users/me/llm-providers/${id}`);
      loadActive();
    } catch (err: any) {
      setError(err.message);
    }
  };

  const startValidation = (id: string) => {
    setValidatingId(id);
    setValidationResults([]);
    setValidationDone(false);

    const es = new EventSource(`/api/users/me/llm-providers/${id}/validate`);
    es.addEventListener('test', (e: any) => {
      const result: ValidationTestResult = JSON.parse(e.data);
      setValidationResults(prev => {
        const existing = prev.findIndex(r => r.test === result.test && r.status === 'running');
        if (existing >= 0 && result.status !== 'running') {
          const updated = [...prev];
          updated[existing] = result;
          return updated;
        }
        return [...prev, result];
      });
    });
    es.addEventListener('done', (e: any) => {
      const data = JSON.parse(e.data);
      setValidationDone(true);
      es.close();
      if (data.all_passed) loadActive();
    });
    es.onerror = () => {
      setValidationDone(true);
      es.close();
    };
  };

  const activeProvider = activeInfo?.provider || 'auto';

  return (
    <div>
      <h2 style="margin-bottom:1rem">Provider Settings</h2>
      <p class="text-muted text-sm mb-md">
        Configure how agents authenticate with Claude. Switch between OAuth, API key, custom providers, or auto-detect.
      </p>

      {/* Active Provider Selector */}
      <div class="card" style="margin-bottom:1rem">
        <div class="card-header">
          <span class="card-title">Active Provider</span>
        </div>
        <div style="padding:1rem">
          <select
            class="input"
            value={activeProvider}
            disabled={switching}
            onChange={(e) => switchProvider((e.target as HTMLSelectElement).value)}
          >
            <option value="auto">Auto (recommended)</option>
            {activeInfo?.has_oauth && <option value="oauth">Claude Subscription (OAuth)</option>}
            {activeInfo?.has_api_key && <option value="api_key">Anthropic API Key</option>}
            <option value="global">Platform Global Key</option>
            {activeInfo?.custom_configs.map(c => (
              <option key={c.id} value={`custom:${c.id}`} disabled={c.validation_status !== 'valid'}>
                {c.label || PROVIDER_LABELS[c.provider_type] || c.provider_type}
                {c.validation_status !== 'valid' ? ' (not validated)' : ''}
              </option>
            ))}
          </select>
          <p class="text-muted text-sm" style="margin-top:0.5rem">
            {activeProvider === 'auto' && 'Auto-detect: tries OAuth → API Key → Global in order.'}
            {activeProvider === 'oauth' && 'Using your Claude subscription (OAuth token).'}
            {activeProvider === 'api_key' && 'Using your Anthropic API key.'}
            {activeProvider === 'global' && 'Using the platform shared key.'}
            {activeProvider.startsWith('custom:') && 'Using custom provider configuration.'}
          </p>
        </div>
      </div>

      {/* OAuth Token Card */}
      <div class="card" style="margin-bottom:1rem">
        <div class="card-header">
          <span class="card-title">Claude CLI OAuth Token</span>
        </div>
        <div style="padding:1rem">
          {cliCreds.exists ? (
            <div class="flex-between mb-md">
              <div>
                <span class="badge">{cliCreds.auth_type}</span>
                <span class="text-muted text-sm" style="margin-left:0.5rem">
                  Updated {timeAgo(cliCreds.updated_at!)}
                </span>
              </div>
              <button class="btn btn-danger btn-sm" onClick={removeOauth}>Remove</button>
            </div>
          ) : (
            <div class="text-muted text-sm mb-md">No token configured</div>
          )}

          <form onSubmit={saveOauth}>
            <div class="form-group">
              <label>{cliCreds.exists ? 'Replace token' : 'Set token'}</label>
              <input
                class="input"
                type="password"
                placeholder="Paste OAuth token..."
                value={oauthToken}
                onInput={(e) => setOauthToken((e.target as HTMLInputElement).value)}
                minLength={10}
              />
            </div>
            {oauthError && <div class="error-msg">{oauthError}</div>}
            {oauthSuccess && <div class="success-msg">{oauthSuccess}</div>}
            <button type="submit" class="btn btn-primary btn-sm" disabled={oauthSaving || oauthToken.length < 10}>
              {oauthSaving ? 'Saving...' : 'Save Token'}
            </button>
          </form>
        </div>
      </div>

      {/* API Key Card */}
      <div class="card" style="margin-bottom:1rem">
        <div class="card-header">
          <span class="card-title">Anthropic API Key</span>
        </div>
        <div style="padding:1rem">
          {existing ? (
            <div class="flex-between mb-md">
              <div>
                <span class="mono text-sm">{existing.key_suffix}</span>
                <span class="text-muted text-sm" style="margin-left:0.5rem">
                  Updated {timeAgo(existing.updated_at)}
                </span>
              </div>
              <button class="btn btn-danger btn-sm" onClick={remove}>Remove</button>
            </div>
          ) : (
            <div class="text-muted text-sm mb-md">No key configured</div>
          )}

          <form onSubmit={save}>
            <div class="form-group">
              <label>{existing ? 'Replace key' : 'Set key'}</label>
              <input
                class="input"
                type="password"
                placeholder="sk-ant-api03-..."
                value={apiKey}
                onInput={(e) => setApiKey((e.target as HTMLInputElement).value)}
                minLength={10}
              />
            </div>
            {error && <div class="error-msg">{error}</div>}
            {success && <div class="success-msg">{success}</div>}
            <button type="submit" class="btn btn-primary btn-sm" disabled={saving || apiKey.length < 10}>
              {saving ? 'Saving...' : 'Save Key'}
            </button>
          </form>
        </div>
      </div>

      {/* Custom Providers */}
      <div class="card">
        <div class="card-header flex-between">
          <span class="card-title">Custom Providers</span>
          <button class="btn btn-primary btn-sm" onClick={() => setShowAddForm(true)}>
            Add Provider
          </button>
        </div>
        <div style="padding:1rem">
          {activeInfo?.custom_configs.length === 0 && !showAddForm && (
            <div class="text-muted text-sm">
              No custom providers configured. Add one to use Bedrock, Vertex AI, Azure Foundry, or a custom endpoint.
            </div>
          )}

          {/* Existing configs */}
          {activeInfo?.custom_configs.map(config => {
            const badge = STATUS_BADGES[config.validation_status] || STATUS_BADGES.untested;
            return (
              <div key={config.id} class="flex-between" style="padding:0.5rem 0;border-bottom:1px solid var(--border)">
                <div>
                  <span style="font-weight:600;font-size:13px">
                    {config.label || PROVIDER_LABELS[config.provider_type]}
                  </span>
                  <span class="badge" style="margin-left:0.5rem">{PROVIDER_LABELS[config.provider_type]}</span>
                  <span class={`badge ${badge.class}`} style="margin-left:0.25rem">{badge.label}</span>
                  {config.model && (
                    <span class="text-muted text-sm" style="margin-left:0.5rem">{config.model}</span>
                  )}
                </div>
                <div style="display:flex;gap:0.25rem">
                  <button
                    class="btn btn-ghost btn-xs"
                    onClick={() => startValidation(config.id)}
                    disabled={validatingId === config.id && !validationDone}
                  >
                    {validatingId === config.id && !validationDone ? 'Testing...' : 'Validate'}
                  </button>
                  {config.validation_status === 'valid' && activeProvider !== `custom:${config.id}` && (
                    <button class="btn btn-ghost btn-xs" onClick={() => switchProvider(`custom:${config.id}`)}>
                      Set Active
                    </button>
                  )}
                  <button class="btn btn-danger btn-xs" onClick={() => deleteCustomProvider(config.id)}>
                    Delete
                  </button>
                </div>
              </div>
            );
          })}

          {/* Validation results */}
          {validatingId && validationResults.length > 0 && (
            <div style="margin-top:0.75rem;padding:0.75rem;background:var(--bg-secondary);border-radius:6px">
              {validationResults.filter(r => r.status !== 'running').map(r => (
                <div key={`${r.test}-${r.status}`} style="font-size:12px;margin-bottom:0.25rem">
                  <span style={`color:${r.status === 'passed' ? 'var(--success)' : 'var(--danger)'}`}>
                    {r.status === 'passed' ? '✓' : '✗'}
                  </span>
                  {' '}{r.name}: {r.detail}
                </div>
              ))}
              {!validationDone && <div class="text-muted text-sm">Running tests...</div>}
            </div>
          )}

          {/* Add form */}
          {showAddForm && (
            <div style="margin-top:1rem;padding:1rem;border:1px solid var(--border);border-radius:6px">
              <div style="display:flex;justify-content:space-between;margin-bottom:0.75rem">
                <span style="font-weight:600">Add Custom Provider</span>
                <button class="btn btn-ghost btn-xs" onClick={() => { setShowAddForm(false); setAddType(null); }}>Cancel</button>
              </div>

              {!addType && (
                <div style="display:grid;grid-template-columns:repeat(2,1fr);gap:0.5rem">
                  {(['bedrock', 'vertex', 'azure_foundry', 'custom_endpoint'] as CustomProviderType[]).map(type => (
                    <div
                      key={type}
                      class="auth-option-card"
                      style="cursor:pointer;padding:0.75rem"
                      onClick={() => { setAddType(type); setAddEnvVars({}); }}
                    >
                      <div style="font-size:13px;font-weight:600">{PROVIDER_LABELS[type]}</div>
                    </div>
                  ))}
                </div>
              )}

              {addType && (
                <div>
                  {addType === 'bedrock' && (
                    <>
                      <div class="form-group">
                        <label>AWS Access Key ID</label>
                        <input class="input" placeholder="AKIA..."
                          value={addEnvVars['AWS_ACCESS_KEY_ID'] || ''}
                          onInput={(e) => setAddEnvVars(v => ({...v, AWS_ACCESS_KEY_ID: (e.target as HTMLInputElement).value}))}
                        />
                      </div>
                      <div class="form-group">
                        <label>AWS Secret Access Key</label>
                        <input class="input" type="password"
                          value={addEnvVars['AWS_SECRET_ACCESS_KEY'] || ''}
                          onInput={(e) => setAddEnvVars(v => ({...v, AWS_SECRET_ACCESS_KEY: (e.target as HTMLInputElement).value}))}
                        />
                      </div>
                      <div class="form-group">
                        <label>AWS Region (optional)</label>
                        <input class="input" placeholder="us-east-1"
                          value={addEnvVars['AWS_REGION'] || ''}
                          onInput={(e) => setAddEnvVars(v => ({...v, AWS_REGION: (e.target as HTMLInputElement).value}))}
                        />
                      </div>
                    </>
                  )}
                  {addType === 'vertex' && (
                    <>
                      <div class="form-group">
                        <label>Vertex Project ID</label>
                        <input class="input" placeholder="my-gcp-project"
                          value={addEnvVars['ANTHROPIC_VERTEX_PROJECT_ID'] || ''}
                          onInput={(e) => setAddEnvVars(v => ({...v, ANTHROPIC_VERTEX_PROJECT_ID: (e.target as HTMLInputElement).value}))}
                        />
                      </div>
                      <div class="form-group">
                        <label>Region (optional)</label>
                        <input class="input" placeholder="global"
                          value={addEnvVars['CLOUD_ML_REGION'] || ''}
                          onInput={(e) => setAddEnvVars(v => ({...v, CLOUD_ML_REGION: (e.target as HTMLInputElement).value}))}
                        />
                      </div>
                    </>
                  )}
                  {addType === 'azure_foundry' && (
                    <div class="form-group">
                      <label>Foundry API Key</label>
                      <input class="input" type="password"
                        value={addEnvVars['ANTHROPIC_FOUNDRY_API_KEY'] || ''}
                        onInput={(e) => setAddEnvVars(v => ({...v, ANTHROPIC_FOUNDRY_API_KEY: (e.target as HTMLInputElement).value}))}
                      />
                    </div>
                  )}
                  {addType === 'custom_endpoint' && (
                    <>
                      <div class="form-group">
                        <label>Base URL</label>
                        <input class="input" placeholder="https://litellm.example.com/v1"
                          value={addEnvVars['ANTHROPIC_BASE_URL'] || ''}
                          onInput={(e) => setAddEnvVars(v => ({...v, ANTHROPIC_BASE_URL: (e.target as HTMLInputElement).value}))}
                        />
                      </div>
                      <div class="form-group">
                        <label>API Key</label>
                        <input class="input" type="password" placeholder="sk-..."
                          value={addEnvVars['ANTHROPIC_API_KEY'] || ''}
                          onInput={(e) => setAddEnvVars(v => ({...v, ANTHROPIC_API_KEY: (e.target as HTMLInputElement).value}))}
                        />
                      </div>
                    </>
                  )}
                  <div class="form-group">
                    <label>Model (optional)</label>
                    <input class="input" placeholder="e.g. us.anthropic.claude-sonnet-4-5-20250929-v2:0"
                      value={addModel}
                      onInput={(e) => setAddModel((e.target as HTMLInputElement).value)}
                    />
                  </div>
                  <div class="form-group">
                    <label>Label (optional)</label>
                    <input class="input" placeholder="My AWS Account"
                      value={addLabel}
                      onInput={(e) => setAddLabel((e.target as HTMLInputElement).value)}
                    />
                  </div>
                  {addError && <div class="error-msg">{addError}</div>}
                  <button class="btn btn-primary btn-sm" disabled={addSaving} onClick={addCustomProvider}>
                    {addSaving ? 'Saving...' : 'Save Provider'}
                  </button>
                </div>
              )}
            </div>
          )}
        </div>
      </div>
    </div>
  );
}
