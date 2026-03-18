import { useState, useRef } from 'preact/hooks';
import { api } from '../lib/api';

type OrgType = 'solo' | 'startup' | 'tech_org' | 'exploring';
type PasskeyPolicy = 'optional' | 'recommended' | 'mandatory';
type AuthMethod = 'oauth' | 'api_key' | null;

interface WizardResponse {
  success: boolean;
}

const ORG_OPTIONS: { type: OrgType; title: string; desc: string }[] = [
  { type: 'solo', title: 'Solo Developer', desc: 'Building on your own' },
  { type: 'startup', title: 'Startup', desc: 'Small team, moving fast' },
  { type: 'tech_org', title: 'Tech Organization', desc: 'Enterprise security & compliance' },
  { type: 'exploring', title: 'Just Exploring', desc: 'Try everything in 2 clicks' },
];

export function Onboarding() {
  const [step, setStep] = useState(1);
  const [orgType, setOrgType] = useState<OrgType | null>(null);
  const [passkey, setPasskey] = useState<PasskeyPolicy>('optional');
  const [submitting, setSubmitting] = useState(false);

  // Provider auth state
  const [authMethod, setAuthMethod] = useState<AuthMethod>(null);
  const [apiKey, setApiKey] = useState('');
  const [keyValid, setKeyValid] = useState<boolean | null>(null);
  const [validating, setValidating] = useState(false);

  // OAuth manual token state
  const [manualToken, setManualToken] = useState('');
  const [tokenVerifying, setTokenVerifying] = useState(false);
  const [tokenValid, setTokenValid] = useState<boolean | null>(null);
  const [tokenError, setTokenError] = useState<string | null>(null);

  // OAuth CLI flow state
  const [cliStarting, setCliStarting] = useState(false);
  const [authSessionId, setAuthSessionId] = useState<string | null>(null);
  const [authUrl, setAuthUrl] = useState<string | null>(null);
  const [cliError, setCliError] = useState(false);
  const [clickedOAuthLink, setClickedOAuthLink] = useState(false);
  const [authCode, setAuthCode] = useState('');
  const [codeVerifying, setCodeVerifying] = useState(false);
  const [codeResult, setCodeResult] = useState<'success' | 'error' | null>(null);

  // Debounce ref for token verification
  const verifyTimer = useRef<ReturnType<typeof setTimeout> | null>(null);

  // Solo/exploring: 2 steps (org type + provider). Others: 3 steps (+ security).
  const totalSteps = orgType === 'solo' || orgType === 'exploring' ? 2 : 3;

  const handleOrgSelect = (type: OrgType) => {
    setOrgType(type);

    if (type === 'startup') setPasskey('recommended');
    else if (type === 'tech_org') setPasskey('mandatory');
    else setPasskey('optional');

    // "Just Exploring" fast path: skip straight to submit
    if (type === 'exploring') {
      submitWizard(type, 'optional');
      return;
    }

    setStep(2);
  };

  // -- API key validation (existing) --
  const validateKey = async () => {
    if (!apiKey.trim()) return;
    setValidating(true);
    setKeyValid(null);
    try {
      const resp = await api.post<{ valid: boolean }>('/api/users/me/provider-keys/validate', { api_key: apiKey });
      setKeyValid(resp.valid);
    } catch {
      setKeyValid(false);
    } finally {
      setValidating(false);
    }
  };

  // -- OAuth token verification --
  const verifyOAuthToken = async (token: string) => {
    if (!token.trim() || token.trim().length < 10) return;
    setTokenVerifying(true);
    setTokenValid(null);
    setTokenError(null);
    try {
      const resp = await api.post<{ valid: boolean; error?: string }>(
        '/api/onboarding/claude-auth/verify-token',
        { token },
      );
      setTokenValid(resp.valid);
      if (!resp.valid && resp.error) setTokenError(resp.error);
    } catch {
      setTokenValid(false);
      setTokenError('Failed to verify token');
    } finally {
      setTokenVerifying(false);
    }
  };

  const handleTokenInput = (value: string) => {
    setManualToken(value);
    setTokenValid(null);
    setTokenError(null);
    // Debounce auto-verify: 800ms after typing stops
    if (verifyTimer.current) clearTimeout(verifyTimer.current);
    if (value.trim().length >= 20) {
      verifyTimer.current = setTimeout(() => verifyOAuthToken(value), 800);
    }
  };

  // -- CLI OAuth flow --
  const startOAuthFlow = async () => {
    if (cliStarting || authUrl) return;
    setCliStarting(true);
    setCliError(false);
    try {
      const resp = await api.post<{ session_id: string; auth_url: string }>(
        '/api/onboarding/claude-auth/start',
      );
      setAuthSessionId(resp.session_id);
      setAuthUrl(resp.auth_url);
    } catch {
      setCliError(true);
    } finally {
      setCliStarting(false);
    }
  };

  const handleSelectOAuth = () => {
    setAuthMethod('oauth');
    // Start CLI in background to get the URL ready
    startOAuthFlow();
  };

  const handleClickOAuthLink = () => {
    setClickedOAuthLink(true);
  };

  const submitCode = async (code: string) => {
    if (!authSessionId || codeVerifying || code.trim().length < 5) return;
    setCodeVerifying(true);
    setCodeResult(null);
    try {
      await api.post(`/api/onboarding/claude-auth/${authSessionId}/code`, { code });
      setCodeResult('success');
    } catch {
      setCodeResult('error');
    } finally {
      setCodeVerifying(false);
    }
  };

  const handleCodeInput = (value: string) => {
    setAuthCode(value);
    setCodeResult(null);
    // Auto-submit when code looks complete (> 10 chars)
    if (value.trim().length > 10) {
      submitCode(value);
    }
  };

  // -- Wizard submission --
  const submitWizard = async (ot: OrgType, pp: PasskeyPolicy) => {
    setSubmitting(true);
    try {
      const body: Record<string, unknown> = {
        org_type: ot,
        passkey_policy: pp,
      };

      // If API key was provided and validated, include it
      if (authMethod === 'api_key' && apiKey.trim()) {
        body.provider_key = apiKey;
      }
      // OAuth token or code flow — tokens are already stored server-side by
      // verify-token or /code endpoints, no need to send again.

      await api.post<WizardResponse>('/api/onboarding/wizard', body);
      window.location.href = '/';
    } catch {
      setSubmitting(false);
    }
  };

  const handleFinish = () => {
    if (!orgType) return;
    submitWizard(orgType, passkey);
  };

  // Whether the OAuth flow is completed (either manual token verified or code flow done)
  const oauthDone = tokenValid === true || codeResult === 'success';
  // Whether user is in the code flow (clicked the link)
  const inCodeFlow = clickedOAuthLink;

  // Step mapping
  const securityStep = orgType === 'solo' || orgType === 'exploring' ? -1 : 2;
  const providerStep = orgType === 'solo' || orgType === 'exploring' ? 2 : 3;

  return (
    <div class="wizard-page">
      <div class="aurora-bg">
        <div class="aurora-blob-3" />
      </div>
      <div class="wizard-panel">
        {/* Progress dots */}
        <div class="wizard-progress">
          {Array.from({ length: totalSteps }, (_, i) => (
            <div
              key={i}
              class={`wizard-dot${i + 1 === step ? ' active' : ''}${i + 1 < step ? ' completed' : ''}`}
            />
          ))}
        </div>

        {/* Step 1: Who are you? */}
        {step === 1 && (
          <div class="wizard-step">
            <h1 class="hero-greeting" style="font-size:1.5rem;margin-bottom:0.5rem">
              Welcome to the Platform
            </h1>
            <p class="text-muted text-sm" style="text-align:center;margin-bottom:1.5rem">
              How will you be using the platform?
            </p>
            <div class="org-type-grid">
              {ORG_OPTIONS.map(opt => (
                <div
                  key={opt.type}
                  class={`org-type-card${orgType === opt.type ? ' selected' : ''}`}
                  onClick={() => handleOrgSelect(opt.type)}
                >
                  <div style="font-size:14px;font-weight:600;color:var(--text-primary);margin-bottom:0.25rem">
                    {opt.title}
                  </div>
                  <div style="font-size:12px;color:var(--text-muted)">{opt.desc}</div>
                </div>
              ))}
            </div>
            {submitting && (
              <div style="text-align:center;color:var(--text-muted);margin-top:1rem">
                Setting up your platform...
              </div>
            )}
          </div>
        )}

        {/* Step 2: Security (Startup + Tech Org only) */}
        {step === securityStep && (
          <div class="wizard-step">
            <h1 class="hero-greeting" style="font-size:1.5rem;margin-bottom:0.5rem">
              Security Settings
            </h1>
            <p class="text-muted text-sm" style="text-align:center;margin-bottom:1.5rem">
              Configure passkey enforcement for your team
            </p>
            <div class="form-group">
              <label>Passkey Enforcement</label>
              <select
                class="input"
                value={passkey}
                onChange={(e) => setPasskey((e.target as HTMLSelectElement).value as PasskeyPolicy)}
              >
                <option value="optional">Optional</option>
                <option value="recommended">Recommended (prompt users)</option>
                <option value="mandatory">Mandatory (require for all users)</option>
              </select>
            </div>
            {orgType === 'tech_org' && passkey !== 'mandatory' && (
              <p style="color:var(--warning);font-size:12px;margin-top:0.5rem">
                Tech organizations typically enforce mandatory passkeys for compliance.
              </p>
            )}
            <div class="wizard-actions">
              <button class="btn btn-ghost" onClick={() => setStep(1)}>Back</button>
              <button class="btn btn-primary" onClick={() => setStep(providerStep)}>Continue</button>
            </div>
          </div>
        )}

        {/* Provider Token Step — OAuth or API Key */}
        {step === providerStep && (
          <div class="wizard-step">
            <h1 class="hero-greeting" style="font-size:1.5rem;margin-bottom:0.5rem">
              Connect to Claude
            </h1>
            <p class="text-muted text-sm" style="text-align:center;margin-bottom:1.5rem">
              Choose how agents authenticate with Claude
            </p>

            {/* Two option cards */}
            <div class="auth-option-grid">
              <div
                class={`auth-option-card${authMethod === 'oauth' ? ' selected' : ''}`}
                onClick={handleSelectOAuth}
              >
                <div style="font-size:14px;font-weight:600;color:var(--text-primary);margin-bottom:0.25rem">
                  Claude Subscription
                </div>
                <div style="font-size:12px;color:var(--text-muted)">
                  Uses your existing Claude Pro/Team plan. No extra cost — counts toward your subscription usage.
                </div>
                <div class="auth-option-badge">Recommended</div>
              </div>

              <div
                class={`auth-option-card${authMethod === 'api_key' ? ' selected' : ''}`}
                onClick={() => setAuthMethod('api_key')}
              >
                <div style="font-size:14px;font-weight:600;color:var(--text-primary);margin-bottom:0.25rem">
                  Anthropic API Key
                </div>
                <div style="font-size:12px;color:var(--text-muted)">
                  Billed separately through the Anthropic API. Pay per token used.
                </div>
              </div>
            </div>

            {/* OAuth flow */}
            {authMethod === 'oauth' && (
              <div class="auth-input-area">
                {/* Completed state */}
                {oauthDone && (
                  <div class="auth-success">
                    <span style="font-size:16px;font-weight:bold">&#10003;</span>
                    Connected to Claude successfully
                  </div>
                )}

                {!oauthDone && (
                  <>
                    {/* Manual OAuth token input */}
                    <div class="form-group">
                      <label>Have an existing OAuth Token?</label>
                      <div style="display:flex;gap:0.5rem">
                        <input
                          class="input"
                          type="password"
                          placeholder="sk-ant-oat01-..."
                          value={manualToken}
                          disabled={inCodeFlow}
                          onInput={(e) => handleTokenInput((e.target as HTMLInputElement).value)}
                        />
                        <button
                          class="btn btn-primary"
                          style="white-space:nowrap"
                          disabled={!manualToken.trim() || tokenVerifying || inCodeFlow}
                          onClick={() => verifyOAuthToken(manualToken)}
                        >
                          {tokenVerifying ? <><span class="spinner" /> Verifying</> : 'Verify'}
                        </button>
                      </div>
                      {tokenValid === true && (
                        <p style="color:var(--success);font-size:12px;margin-top:0.5rem">
                          Token verified and saved
                        </p>
                      )}
                      {tokenValid === false && (
                        <p style="color:var(--danger);font-size:12px;margin-top:0.5rem">
                          {tokenError || 'Invalid token'}
                        </p>
                      )}
                    </div>

                    {/* Separator */}
                    <div style="display:flex;align-items:center;gap:0.75rem;margin:1rem 0">
                      <div style="flex:1;height:1px;background:var(--border)" />
                      <span style="font-size:12px;color:var(--text-muted)">or</span>
                      <div style="flex:1;height:1px;background:var(--border)" />
                    </div>

                    {/* OAuth CLI flow button */}
                    <div style="margin-bottom:0.75rem">
                      {cliStarting && !authUrl && (
                        <button class="btn btn-primary" disabled style="display:inline-flex;align-items:center;gap:0.5rem">
                          <span class="spinner" /> Connecting to Claude...
                        </button>
                      )}
                      {cliError && !authUrl && (
                        <div>
                          <p style="color:var(--danger);font-size:12px;margin-bottom:0.5rem">
                            Could not start Claude login process. Paste an existing token above instead.
                          </p>
                          <button class="btn btn-ghost" onClick={startOAuthFlow}>
                            Retry
                          </button>
                        </div>
                      )}
                      {authUrl && !clickedOAuthLink && (
                        <a
                          href={authUrl}
                          target="_blank"
                          rel="noopener noreferrer"
                          class="btn btn-primary"
                          style="display:inline-block"
                          onClick={handleClickOAuthLink}
                        >
                          Open OAuth Page in New Tab
                        </a>
                      )}
                      {!cliStarting && !cliError && !authUrl && (
                        <button class="btn btn-ghost" disabled style="opacity:0.5">
                          Waiting for OAuth URL...
                        </button>
                      )}
                    </div>

                    {/* After clicking OAuth link: show auth code input */}
                    {clickedOAuthLink && (
                      <div class="form-group">
                        <label>Authentication Code</label>
                        <p style="font-size:12px;color:var(--text-muted);margin-bottom:0.5rem">
                          After authenticating on claude.ai, paste the code shown on the callback page.
                        </p>
                        <div style="display:flex;gap:0.5rem">
                          <input
                            class="input"
                            placeholder="Paste authentication code..."
                            value={authCode}
                            onInput={(e) => handleCodeInput((e.target as HTMLInputElement).value)}
                          />
                          <button
                            class="btn btn-primary"
                            style="white-space:nowrap"
                            disabled={!authCode.trim() || codeVerifying}
                            onClick={() => submitCode(authCode)}
                          >
                            {codeVerifying ? <><span class="spinner" /> Verifying</> : 'Verify'}
                          </button>
                        </div>
                        {codeResult === 'success' && (
                          <p style="color:var(--success);font-size:12px;margin-top:0.5rem">
                            Authentication successful
                          </p>
                        )}
                        {codeResult === 'error' && (
                          <p style="color:var(--danger);font-size:12px;margin-top:0.5rem">
                            Verification failed — check the code and try again
                          </p>
                        )}
                      </div>
                    )}
                  </>
                )}
              </div>
            )}

            {/* API Key flow */}
            {authMethod === 'api_key' && (
              <div class="auth-input-area">
                <div class="form-group">
                  <label>Anthropic API Key</label>
                  <div style="display:flex;gap:0.5rem">
                    <input
                      class="input"
                      type="password"
                      placeholder="sk-ant-api03-..."
                      value={apiKey}
                      onInput={(e) => { setApiKey((e.target as HTMLInputElement).value); setKeyValid(null); }}
                    />
                    <button
                      class="btn btn-primary"
                      onClick={validateKey}
                      disabled={!apiKey.trim() || validating}
                    >
                      {validating ? 'Checking...' : 'Validate'}
                    </button>
                  </div>
                  {keyValid === true && (
                    <p style="color:var(--success);font-size:12px;margin-top:0.5rem">
                      API key verified
                    </p>
                  )}
                  {keyValid === false && (
                    <p style="color:var(--danger);font-size:12px;margin-top:0.5rem">
                      Invalid API key
                    </p>
                  )}
                </div>
              </div>
            )}

            <button
              class="btn btn-ghost text-sm"
              style="width:100%;margin-top:0.25rem"
              onClick={handleFinish}
            >
              Skip — I'll do this later
            </button>

            <div class="wizard-actions">
              <button class="btn btn-ghost" onClick={() => setStep(securityStep > 0 ? securityStep : 1)}>
                Back
              </button>
              <button
                class="btn btn-primary"
                onClick={handleFinish}
                disabled={submitting}
              >
                {submitting ? 'Finishing...' : 'Finish Setup'}
              </button>
            </div>
          </div>
        )}
      </div>
    </div>
  );
}
