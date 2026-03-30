import { useState, useEffect } from 'preact/hooks';
import { api } from '../lib/api';

interface StagingStatus {
  diverged: boolean;
  staging_image: string | null;
  prod_image: string | null;
  staging_sha: string | null;
  prod_sha: string | null;
}

interface Props {
  projectId: string;
}

export function StagingPromoteBar({ projectId }: Props) {
  const [status, setStatus] = useState<StagingStatus | null>(null);
  const [promoting, setPromoting] = useState(false);
  const [showConfirm, setShowConfirm] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    api.get<StagingStatus>(`/api/projects/${projectId}/staging-status`)
      .then(setStatus)
      .catch(() => {});
  }, [projectId]);

  if (!status || !status.diverged) return null;

  const doPromote = async () => {
    setPromoting(true);
    setError(null);
    try {
      await api.post(`/api/projects/${projectId}/promote-staging`, {});
      setStatus(prev => prev ? { ...prev, diverged: false } : null);
      setShowConfirm(false);
    } catch (err: any) {
      setError(err?.message || 'Promotion failed');
    } finally {
      setPromoting(false);
    }
  };

  const shortSha = (sha: string | null) => sha ? sha.slice(0, 8) : '—';

  return (
    <div class="promote-bar">
      <div class="promote-info">
        <span class="promote-badge">Staging ahead</span>
        <span class="promote-detail">
          staging: {shortSha(status.staging_sha)} → prod: {shortSha(status.prod_sha)}
        </span>
      </div>
      <button
        class="btn btn-sm btn-warning"
        onClick={(e: Event) => { e.stopPropagation(); setShowConfirm(true); }}
      >
        Promote to Prod
      </button>

      {showConfirm && (
        <div class="modal-overlay" onClick={() => setShowConfirm(false)}>
          <div class="modal-content" onClick={(e: Event) => e.stopPropagation()}>
            <h3>Promote to Production</h3>
            <p>This will deploy the staging version to production. This action affects live traffic.</p>
            <div class="promote-diff">
              <div><strong>Staging:</strong> {shortSha(status.staging_sha)}</div>
              <div><strong>Production:</strong> {shortSha(status.prod_sha)}</div>
            </div>
            {error && <div class="form-error">{error}</div>}
            <div class="modal-actions">
              <button class="btn btn-sm" onClick={() => setShowConfirm(false)}>Cancel</button>
              <button class="btn btn-sm btn-danger" onClick={doPromote} disabled={promoting}>
                {promoting ? 'Promoting...' : 'Confirm Promotion'}
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
