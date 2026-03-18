import { useState, useEffect, useRef } from 'preact/hooks';
import { api, type ListResponse } from '../lib/api';
import type { Project, AgentSession, Deployment, IframePanel } from '../lib/types';
import { timeAgo } from '../lib/format';

interface Props {
  project: Project;
}

export function ProjectCard({ project }: Props) {
  const [session, setSession] = useState<AgentSession | null>(null);
  const [deployment, setDeployment] = useState<Deployment | null>(null);
  const [progressText, setProgressText] = useState<string | null>(null);
  const [deployIframes, setDeployIframes] = useState<IframePanel[]>([]);
  const [activePreviewIdx, setActivePreviewIdx] = useState(0);
  const [visible, setVisible] = useState(false);
  const cardRef = useRef<HTMLDivElement>(null);

  // IntersectionObserver — only fetch data when card is visible
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

  // Fetch per-card data when visible
  useEffect(() => {
    if (!visible) return;
    api.get<ListResponse<AgentSession>>(`/api/projects/${project.id}/sessions?status=running&limit=1`)
      .then(r => { if (r.items.length > 0) setSession(r.items[0]); })
      .catch(() => {});
    api.get<ListResponse<Deployment>>(`/api/projects/${project.id}/deployments?limit=1`)
      .then(r => { if (r.items.length > 0) setDeployment(r.items[0]); })
      .catch(() => {});
  }, [visible, project.id]);

  // Fetch progress if running session exists
  useEffect(() => {
    if (!session) return;
    api.get<{ message: string }>(`/api/projects/${project.id}/sessions/${session.id}/progress`)
      .then(r => setProgressText(r.message))
      .catch(() => {});
  }, [session, project.id]);

  // Dashboard shows prod deploy previews only (session previews live on SessionDetail)
  useEffect(() => {
    if (!visible || !deployment) return;
    api.get<IframePanel[]>(`/api/projects/${project.id}/deploy-preview/iframes`)
      .then(setDeployIframes)
      .catch(() => setDeployIframes([]));
  }, [visible, deployment, project.id]);

  const activeIframes = deployIframes;
  const clampedIdx = Math.min(activePreviewIdx, Math.max(0, activeIframes.length - 1));
  const currentIframe = activeIframes[clampedIdx];
  const previewUrl = currentIframe?.preview_url ?? null;
  const displayName = project.display_name || project.name;
  const initial = displayName.charAt(0).toUpperCase();

  const statusColor = (s: string): string => {
    if (s === 'healthy' || s === 'success' || s === 'running') return 'var(--success)';
    if (s === 'degraded' || s === 'syncing' || s === 'pending') return 'var(--warning)';
    if (s === 'failure' || s === 'failed' || s === 'error') return 'var(--danger)';
    return 'var(--text-muted)';
  };

  return (
    <div ref={cardRef} class="project-card" onClick={() => { window.location.href = `/projects/${project.id}`; }}>
      {/* Left: preview */}
      <div class="project-card-preview">
        {previewUrl ? (
          <iframe src={previewUrl} tabIndex={-1} loading="lazy" />
        ) : (
          <div class="project-card-preview-placeholder"
            style={`background: linear-gradient(135deg, ${gradientFor(displayName)})`}>
            {initial}
          </div>
        )}
        {activeIframes.length > 1 && (
          <div class="project-card-preview-dots">
            {activeIframes.map((_, i) => (
              <button key={i} class={`preview-dot ${i === clampedIdx ? 'active' : ''}`}
                onClick={(e: Event) => { e.stopPropagation(); setActivePreviewIdx(i); }} />
            ))}
          </div>
        )}
        <div class="project-card-preview-overlay">
          {activeIframes.length > 1 && (
            <button class="preview-arrow preview-arrow-left"
              onClick={(e: Event) => { e.stopPropagation(); setActivePreviewIdx((clampedIdx - 1 + activeIframes.length) % activeIframes.length); }}>
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
          {activeIframes.length > 1 && (
            <button class="preview-arrow preview-arrow-right"
              onClick={(e: Event) => { e.stopPropagation(); setActivePreviewIdx((clampedIdx + 1) % activeIframes.length); }}>
              &#8250;
            </button>
          )}
        </div>
      </div>

      {/* Right: info */}
      <div class="project-card-info">
        <div class="project-card-name">
          <a href={`/projects/${project.id}`} onClick={(e: Event) => e.stopPropagation()}>
            {displayName}
          </a>
        </div>

        {/* Session status */}
        <div class="project-card-row" title={session && progressText ? progressText : undefined}>
          {session ? (
            <>
              <span class="project-card-spinner" />
              <span style="color:var(--text-primary);cursor:help">{progressText || 'Running...'}</span>
            </>
          ) : (
            <span style="color:var(--text-muted)">No active session</span>
          )}
        </div>

        {/* Deployment health */}
        <div class="project-card-row">
          {deployment ? (
            <>
              <span class="status-dot" style={`background:${statusColor(deployment.current_status)}`} />
              <span>{deployment.environment}: {deployment.current_status}</span>
            </>
          ) : (
            <span style="color:var(--text-muted)">Not deployed</span>
          )}
        </div>

        {/* Metrics placeholder */}
        <div class="project-card-metrics">
          -- req/s | -- ms p99
        </div>
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
