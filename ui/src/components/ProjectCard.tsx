import { useState, useEffect, useRef } from 'preact/hooks';
import { api, type ListResponse } from '../lib/api';
import type { Project, AgentSession, Pipeline, IframePanel } from '../lib/types';
import { timeAgo } from '../lib/format';
import { StagingPromoteBar } from './StagingPromoteBar';
import { FeatureFlagsPanel } from './FeatureFlagsPanel';

interface DeployRelease {
  id: string;
  target_id: string;
  phase: string;
  health: string;
  strategy: string;
  image_ref: string;
  environment: string;
}

interface Props {
  project: Project;
}

export function ProjectCard({ project }: Props) {
  const [session, setSession] = useState<AgentSession | null>(null);
  const [releases, setReleases] = useState<DeployRelease[]>([]);
  const [pipeline, setPipeline] = useState<Pipeline | null>(null);
  const [progressText, setProgressText] = useState<string | null>(null);
  const [deployIframes, setDeployIframes] = useState<(IframePanel & { env?: string })[]>([]);
  const [activePreviewIdx, setActivePreviewIdx] = useState(0);
  const [errorCount, setErrorCount] = useState<number | null>(null);
  const [visible, setVisible] = useState(false);
  const cardRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const el = cardRef.current;
    if (!el) return;
    const observer = new IntersectionObserver(
      ([entry]) => { if (entry.isIntersecting) { setVisible(true); observer.disconnect(); } },
      { rootMargin: '100px' }
    );
    observer.observe(el);
    return () => observer.disconnect();
  }, []);

  useEffect(() => {
    if (!visible) return;
    api.get<ListResponse<AgentSession>>(`/api/projects/${project.id}/sessions?status=running&limit=1`)
      .then(r => { if (r.items.length > 0) setSession(r.items[0]); })
      .catch(() => {});
    api.get<ListResponse<DeployRelease>>(`/api/projects/${project.id}/deploy-releases?limit=5`)
      .then(r => setReleases(r.items))
      .catch(() => {});
    api.get<ListResponse<Pipeline>>(`/api/projects/${project.id}/pipelines?limit=1`)
      .then(r => { if (r.items.length > 0) setPipeline(r.items[0]); })
      .catch(() => {});
    api.get<ListResponse<unknown>>(`/api/observe/logs?project_id=${project.id}&level=error&range=1h&limit=0`)
      .then(r => setErrorCount(r.total))
      .catch(() => {});
  }, [visible, project.id]);

  useEffect(() => {
    if (!session) return;
    api.get<{ message: string }>(`/api/projects/${project.id}/sessions/${session.id}/progress`)
      .then(r => setProgressText(r.message))
      .catch(() => {});
  }, [session, project.id]);

  // Fetch previews from both staging and production
  useEffect(() => {
    if (!visible) return;
    Promise.all([
      api.get<IframePanel[]>(`/api/projects/${project.id}/deploy-preview/iframes?env=production`).catch(() => [] as IframePanel[]),
      api.get<IframePanel[]>(`/api/projects/${project.id}/deploy-preview/iframes?env=staging`).catch(() => [] as IframePanel[]),
    ]).then(([prod, staging]) => {
      setDeployIframes([
        ...prod.map(p => ({ ...p, env: 'prod' as const })),
        ...staging.map(s => ({ ...s, env: 'staging' as const })),
      ]);
    });
  }, [visible, project.id]);

  const clampedIdx = Math.min(activePreviewIdx, Math.max(0, deployIframes.length - 1));
  const currentIframe = deployIframes[clampedIdx];
  const previewUrl = currentIframe?.preview_url ?? null;
  const displayName = project.display_name || project.name;
  const initial = displayName.charAt(0).toUpperCase();

  // Derive deployment summary from releases
  const latestByEnv = new Map<string, DeployRelease>();
  for (const r of releases) {
    if (!latestByEnv.has(r.environment)) latestByEnv.set(r.environment, r);
  }
  const hasDeployment = latestByEnv.size > 0;

  const statusColor = (s: string): string => {
    if (s === 'healthy' || s === 'success' || s === 'running' || s === 'completed') return 'var(--success)';
    if (s === 'degraded' || s === 'syncing' || s === 'pending' || s === 'progressing' || s === 'promoting') return 'var(--warning)';
    if (s === 'failure' || s === 'failed' || s === 'error' || s === 'cancelled' || s === 'rolled_back') return 'var(--danger)';
    return 'var(--text-muted)';
  };

  const pipelineIcon = (status: string) => {
    if (status === 'success') return '\u2713';
    if (status === 'running' || status === 'pending') return '\u21BB';
    if (status === 'failure' || status === 'cancelled') return '\u2717';
    return '\u00B7';
  };

  return (
    <div ref={cardRef} class="project-card" onClick={() => { window.location.href = `/projects/${project.id}`; }}>
      {/* Left: preview */}
      <div class="project-card-preview">
        {previewUrl ? (
          <iframe src={previewUrl} tabIndex={-1} loading="lazy" sandbox="allow-scripts allow-same-origin allow-forms allow-popups" />
        ) : (
          <div class="project-card-preview-placeholder"
            style={`background: linear-gradient(135deg, ${gradientFor(displayName)})`}>
            {initial}
          </div>
        )}
        {deployIframes.length > 1 && (
          <div class="project-card-preview-dots">
            {deployIframes.map((f, i) => (
              <button key={i}
                class={`preview-dot ${i === clampedIdx ? 'active' : ''}`}
                title={f.env || ''}
                onClick={(e: Event) => { e.stopPropagation(); setActivePreviewIdx(i); }} />
            ))}
          </div>
        )}
        {currentIframe?.env && (
          <div class="preview-env-badge">{currentIframe.env}</div>
        )}
        <div class="project-card-preview-overlay">
          {deployIframes.length > 1 && (
            <button class="preview-arrow preview-arrow-left"
              onClick={(e: Event) => { e.stopPropagation(); setActivePreviewIdx((clampedIdx - 1 + deployIframes.length) % deployIframes.length); }}>
              &#8249;
            </button>
          )}
          <div class="preview-overlay-center">
            <a href={`/projects/${project.id}`} class="btn btn-sm"
              onClick={(e: Event) => e.stopPropagation()}>
              Open Dashboard
            </a>
            {previewUrl && (
              <button class="btn btn-sm btn-primary"
                onClick={(e: Event) => { e.stopPropagation(); window.open(previewUrl, '_blank'); }}>
                Open Website
              </button>
            )}
          </div>
          {deployIframes.length > 1 && (
            <button class="preview-arrow preview-arrow-right"
              onClick={(e: Event) => { e.stopPropagation(); setActivePreviewIdx((clampedIdx + 1) % deployIframes.length); }}>
              &#8250;
            </button>
          )}
        </div>
      </div>

      {/* Center: status */}
      <div class="project-card-info">
        <div class="project-card-name">
          <a href={`/projects/${project.id}`} onClick={(e: Event) => e.stopPropagation()}>
            {displayName}
          </a>
          {errorCount != null && errorCount > 0 && (
            <span class="badge badge-danger" title={`${errorCount} errors in last hour`}>
              {errorCount} err
            </span>
          )}
        </div>

        <div class="project-card-status-grid">
          {/* Session */}
          <div class="project-card-row" title={session && progressText ? progressText : undefined}>
            {session ? (
              <>
                <span class="project-card-spinner" />
                <span style="color:var(--text-primary)">{progressText || 'Agent running...'}</span>
              </>
            ) : (
              <span style="color:var(--text-muted)">No active session</span>
            )}
          </div>

          {/* Pipeline */}
          <div class="project-card-row">
            {pipeline ? (
              <>
                <span style={`color:${statusColor(pipeline.status)};font-weight:600`}>
                  {pipelineIcon(pipeline.status)}
                </span>
                <span>Build: {pipeline.status}</span>
                <span class="text-muted" style="margin-left:auto;font-size:0.75rem">{timeAgo(pipeline.created_at)}</span>
              </>
            ) : (
              <span style="color:var(--text-muted)">No builds</span>
            )}
          </div>

          {/* Deployments */}
          <div class="project-card-row">
            {hasDeployment ? (
              <div class="deploy-envs">
                {Array.from(latestByEnv.entries()).map(([env, r]) => (
                  <span key={env} class="deploy-env-tag">
                    <span class="status-dot" style={`background:${statusColor(r.health || r.phase)}`} />
                    {env}: {r.phase}
                  </span>
                ))}
              </div>
            ) : (
              <span style="color:var(--text-muted)">Not deployed</span>
            )}
          </div>
        </div>

        <StagingPromoteBar projectId={project.id} />
      </div>

      {/* Right: feature flags */}
      <div class="project-card-flags" onClick={(e: Event) => e.stopPropagation()}>
        <FeatureFlagsPanel projectId={project.id} projectName="" />
      </div>
    </div>
  );
}

function gradientFor(name: string): string {
  const colors = [
    ['#3b82f6', '#1e3a5f'],
    ['#a855f7', '#3b1f6e'],
    ['#ec4899', '#5f1e3a'],
    ['#f97316', '#5f3b1e'],
    ['#22c55e', '#14532d'],
    ['#06b6d4', '#164e63'],
  ];
  let hash = 0;
  for (let i = 0; i < name.length; i++) hash = ((hash << 5) - hash + name.charCodeAt(i)) | 0;
  const pair = colors[Math.abs(hash) % colors.length];
  return `${pair[0]}33, ${pair[1]}33`;
}
