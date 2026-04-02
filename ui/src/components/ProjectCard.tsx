import { useState, useEffect, useRef, useCallback } from 'preact/hooks';
import { api, type ListResponse } from '../lib/api';
import type { Project, AgentSession, Pipeline, Issue, MergeRequest, Deployment, IframePanel } from '../lib/types';
import { timeAgo } from '../lib/format';
import { Badge } from './Badge';
import { StagingPromoteBar } from './StagingPromoteBar';
import { FeatureFlagsPanel } from './FeatureFlagsPanel';
import {
  FilesTab, IssuesTab, MRsTab, BuildsTab, UiPreviewsTab,
  DeploymentsTab, SessionsTab, LogsTab, SkillsTab, WebhooksTab,
  SettingsTab, SecretsTab, DocsTab, ObserveTab,
} from './ProjectTabs';

interface DeployRelease {
  id: string;
  target_id: string;
  phase: string;
  health: string;
  strategy: string;
  image_ref: string;
  environment: string;
}

export const ALL_TABS = [
  'files', 'issues', 'mrs', 'builds', 'ui', 'docs',
  'deploys', 'observe', 'sessions', 'skills', 'webhooks', 'settings', 'secrets',
] as const;

export type CardTab = typeof ALL_TABS[number] | null;

const TAB_LABELS: Record<string, string> = {
  files: 'Files', issues: 'Issues', mrs: 'MRs', builds: 'Builds',
  ui: 'UI', docs: 'Docs', deploys: 'Deploys', observe: 'Observe',
  sessions: 'Sessions', skills: 'Skills', webhooks: 'Webhooks',
  settings: 'Settings', secrets: 'Secrets',
};

// Map URL tab names to internal tab names (deploys <-> deployments)
const URL_TAB_MAP: Record<string, string> = { deployments: 'deploys' };
const TAB_URL_MAP: Record<string, string> = { deploys: 'deployments' };

export function parseTabFromUrl(tab?: string): CardTab {
  if (!tab) return null;
  const mapped = URL_TAB_MAP[tab] || tab;
  return (ALL_TABS as readonly string[]).includes(mapped) ? mapped as CardTab : null;
}

interface Props {
  project: Project;
  initialExpanded?: boolean;
  initialTab?: CardTab;
}

export function ProjectCard({ project, initialExpanded, initialTab }: Props) {
  const [session, setSession] = useState<AgentSession | null>(null);
  const [releases, setReleases] = useState<DeployRelease[]>([]);
  const [pipeline, setPipeline] = useState<Pipeline | null>(null);
  const [progressText, setProgressText] = useState<string | null>(null);
  const [deployIframes, setDeployIframes] = useState<(IframePanel & { env?: string })[]>([]);
  const [activePreviewIdx, setActivePreviewIdx] = useState(0);
  const [errorCount, setErrorCount] = useState<number | null>(null);
  const [visible, setVisible] = useState(false);
  const [expanded, setExpanded] = useState(initialExpanded ?? false);
  const [activeTab, setActiveTab] = useState<CardTab>(initialTab ?? null);
  const [issues, setIssues] = useState<Issue[] | null>(null);
  const [mrs, setMrs] = useState<MergeRequest[] | null>(null);
  const [recentPipelines, setRecentPipelines] = useState<Pipeline[] | null>(null);
  const [projectState, setProjectState] = useState<Project>(project);
  const [showSticky, setShowSticky] = useState(false);
  const cardRef = useRef<HTMLDivElement>(null);

  // URL navigation — use pushState with /projects/:id/:tab paths
  const navigate = useCallback((exp: boolean, tab: CardTab) => {
    if (exp && tab) {
      const urlTab = TAB_URL_MAP[tab] || tab;
      history.pushState(null, '', `/projects/${project.id}/${urlTab}`);
    } else if (exp) {
      history.pushState(null, '', `/projects/${project.id}`);
    } else {
      history.pushState(null, '', '/');
    }
    // Dispatch popstate so preact-router picks up the change
    window.dispatchEvent(new PopStateEvent('popstate'));
  }, [project.id]);

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

  // Show sticky compact header when card top scrolls above viewport
  useEffect(() => {
    if (!expanded || !activeTab) { setShowSticky(false); return; }
    const handleScroll = () => {
      const card = cardRef.current;
      if (!card) return;
      const rect = card.getBoundingClientRect();
      setShowSticky(rect.top < -80);
    };
    window.addEventListener('scroll', handleScroll, { passive: true });
    return () => window.removeEventListener('scroll', handleScroll);
  }, [expanded, activeTab]);

  useEffect(() => {
    if (!visible) return;
    api.get<ListResponse<AgentSession>>(`/api/projects/${project.id}/sessions?status=running&limit=1`)
      .then(r => { if (r.items.length > 0) setSession(r.items[0]); }).catch(() => {});
    api.get<ListResponse<DeployRelease>>(`/api/projects/${project.id}/deploy-releases?limit=5`)
      .then(r => setReleases(r.items)).catch(() => {});
    api.get<ListResponse<Pipeline>>(`/api/projects/${project.id}/pipelines?limit=1`)
      .then(r => { if (r.items.length > 0) setPipeline(r.items[0]); }).catch(() => {});
    api.get<ListResponse<unknown>>(`/api/observe/logs?project_id=${project.id}&level=error&range=1h&limit=0`)
      .then(r => setErrorCount(r.total)).catch(() => {});
  }, [visible, project.id]);

  useEffect(() => {
    if (!session) return;
    api.get<{ message: string }>(`/api/projects/${project.id}/sessions/${session.id}/progress`)
      .then(r => setProgressText(r.message)).catch(() => {});
  }, [session, project.id]);

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

  // Load detail data on expand
  useEffect(() => {
    if (!expanded) return;
    if (issues === null) {
      api.get<ListResponse<Issue>>(`/api/projects/${project.id}/issues?limit=5&sort=created_at&order=desc`)
        .then(r => setIssues(r.items)).catch(() => setIssues([]));
    }
    if (mrs === null) {
      api.get<ListResponse<MergeRequest>>(`/api/projects/${project.id}/merge-requests?limit=5&sort=created_at&order=desc`)
        .then(r => setMrs(r.items)).catch(() => setMrs([]));
    }
    if (recentPipelines === null) {
      api.get<ListResponse<Pipeline>>(`/api/projects/${project.id}/pipelines?limit=5`)
        .then(r => setRecentPipelines(r.items)).catch(() => setRecentPipelines([]));
    }
  }, [expanded, project.id]);

  const clampedIdx = Math.min(activePreviewIdx, Math.max(0, deployIframes.length - 1));
  const currentIframe = deployIframes[clampedIdx];
  const previewUrl = currentIframe?.preview_url ?? null;
  const displayName = projectState.display_name || projectState.name;
  const initial = displayName.charAt(0).toUpperCase();

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

  const toggleExpand = (e: Event) => {
    const target = e.target as HTMLElement;
    if (target.closest('a') || target.closest('button') || target.closest('.project-card-preview') || target.closest('.project-card-flags')) return;
    const next = !expanded;
    setExpanded(next);
    if (!next) { setActiveTab(null); }
    navigate(next, null);
  };

  const selectTab = (tab: CardTab) => {
    const next = activeTab === tab ? null : tab;
    setActiveTab(next);
    if (!expanded) setExpanded(true);
    navigate(true, next);
  };

  const issueStatusDot = (s: string) => {
    if (s === 'open') return 'var(--success)';
    if (s === 'closed') return 'var(--text-muted)';
    return 'var(--warning)';
  };

  const mrStatusDot = (s: string) => {
    if (s === 'merged') return 'var(--accent)';
    if (s === 'open') return 'var(--success)';
    if (s === 'closed') return 'var(--danger)';
    return 'var(--text-muted)';
  };

  const renderTabContent = () => {
    switch (activeTab) {
      case 'files': return <FilesTab projectId={project.id} defaultBranch={projectState.default_branch} />;
      case 'issues': return <IssuesTab projectId={project.id} />;
      case 'mrs': return <MRsTab projectId={project.id} />;
      case 'builds': return <BuildsTab projectId={project.id} />;
      case 'ui': return <UiPreviewsTab projectId={project.id} defaultBranch={projectState.default_branch} />;
      case 'docs': return <DocsTab projectId={project.id} defaultBranch={projectState.default_branch} />;
      case 'deploys': return <DeploymentsTab projectId={project.id} />;
      case 'observe': return <ObserveTab projectId={project.id} />;
      case 'sessions': return <SessionsTab projectId={project.id} />;
      case 'skills': return <SkillsTab projectId={project.id} />;
      case 'webhooks': return <WebhooksTab projectId={project.id} />;
      case 'settings': return <SettingsTab project={projectState} onUpdate={setProjectState} />;
      case 'secrets': return <SecretsTab projectId={project.id} />;
      default: return null;
    }
  };

  return (
    <div ref={cardRef} class={`project-card-wrapper${expanded ? ' expanded' : ''}${activeTab ? ' has-tab' : ''}`}>
      {/* Sticky compact header — shown when scrolled past card top */}
      {showSticky && (
        <div class="project-card-sticky"
          onClick={() => { window.scrollTo({ top: (cardRef.current?.offsetTop || 0) - 20, behavior: 'smooth' }); }}>
          <span class="project-card-sticky-name">{displayName}</span>
          <span class="project-card-sticky-session">
            {session ? (progressText || 'Agent running...') : 'No active session'}
          </span>
          <div class="project-card-sticky-tabs">
            {ALL_TABS.map(t => (
              <button key={t} class={`project-card-tab-btn${activeTab === t ? ' active' : ''}`}
                onClick={(e: Event) => { e.stopPropagation(); selectTab(t); }}>
                {TAB_LABELS[t]}
              </button>
            ))}
          </div>
        </div>
      )}
      <div class="project-card" onClick={toggleExpand}>
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
            <span>{displayName}</span>
            {errorCount != null && errorCount > 0 && (
              <span class="badge badge-danger" title={`${errorCount} errors in last hour`}>
                {errorCount} err
              </span>
            )}
          </div>

          <div class="project-card-status-grid">
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

      {/* Expandable detail row */}
      <div class="project-card-expand">
        <div class="project-card-expand-inner">
          <div class="project-card-detail">
            <div class="project-card-detail-section">
              <h4>Issues</h4>
              {issues === null ? (
                <span class="empty-hint">Loading...</span>
              ) : issues.length === 0 ? (
                <span class="empty-hint">No issues yet</span>
              ) : (
                <ul>
                  {issues.map(i => (
                    <li key={i.id}>
                      <span class="status-dot" style={`background:${issueStatusDot(i.status)}`} />
                      <a href={`/projects/${project.id}/issues/${i.number}`}>#{i.number} {i.title}</a>
                    </li>
                  ))}
                </ul>
              )}
            </div>

            <div class="project-card-detail-section">
              <h4>Merge Requests</h4>
              {mrs === null ? (
                <span class="empty-hint">Loading...</span>
              ) : mrs.length === 0 ? (
                <span class="empty-hint">No merge requests yet</span>
              ) : (
                <ul>
                  {mrs.map(m => (
                    <li key={m.id}>
                      <span class="status-dot" style={`background:${mrStatusDot(m.status)}`} />
                      <a href={`/projects/${project.id}/merge-requests/${m.number}`}>!{m.number} {m.title}</a>
                    </li>
                  ))}
                </ul>
              )}
            </div>

            <div class="project-card-detail-section">
              <h4>Recent Builds</h4>
              {recentPipelines === null ? (
                <span class="empty-hint">Loading...</span>
              ) : recentPipelines.length === 0 ? (
                <span class="empty-hint">No builds yet</span>
              ) : (
                <ul>
                  {recentPipelines.map(p => (
                    <li key={p.id}>
                      <span style={`color:${statusColor(p.status)};font-weight:600;font-size:0.75rem`}>
                        {pipelineIcon(p.status)}
                      </span>
                      <a href={`/projects/${project.id}/pipelines/${p.id}`}>
                        {p.git_ref}{p.commit_sha ? ` (${p.commit_sha.slice(0, 7)})` : ''}
                      </a>
                      <span style="margin-left:auto;font-size:0.7rem;color:var(--text-muted);flex-shrink:0">{timeAgo(p.created_at)}</span>
                    </li>
                  ))}
                </ul>
              )}
            </div>

            <div class="project-card-detail-actions">
              {ALL_TABS.map(t => (
                <button key={t} class={`project-card-tab-btn${activeTab === t ? ' active' : ''}`}
                  onClick={() => selectTab(t)}>
                  {TAB_LABELS[t]}
                </button>
              ))}
            </div>
          </div>
        </div>
      </div>

      {/* Row 3: Tab content */}
      <div class={`project-card-tab-expand${activeTab ? ' open' : ''}`}>
        <div class="project-card-tab-inner">
          <div class="project-card-tab-content">
            {renderTabContent()}
          </div>
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
