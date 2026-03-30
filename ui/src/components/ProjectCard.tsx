import { useState, useEffect, useRef, useCallback } from 'preact/hooks';
import { api, type ListResponse } from '../lib/api';
import type { Project, AgentSession, Pipeline, Issue, MergeRequest, Deployment, TreeEntry, BranchInfo, IframePanel } from '../lib/types';
import { timeAgo } from '../lib/format';
import { Badge } from './Badge';
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

export type CardTab = 'files' | 'issues' | 'mrs' | 'builds' | 'deploys' | 'sessions' | null;

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
  const cardRef = useRef<HTMLDivElement>(null);

  // URL sync — replaceState so we don't pollute history
  const updateUrl = useCallback((exp: boolean, tab: CardTab) => {
    if (exp && tab) {
      const url = new URL(window.location.href);
      url.searchParams.set('p', project.id);
      url.searchParams.set('tab', tab);
      history.replaceState(null, '', url.toString());
    } else if (exp) {
      const url = new URL(window.location.href);
      url.searchParams.set('p', project.id);
      url.searchParams.delete('tab');
      history.replaceState(null, '', url.toString());
    } else {
      const url = new URL(window.location.href);
      url.searchParams.delete('p');
      url.searchParams.delete('tab');
      history.replaceState(null, '', url.pathname);
    }
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

  const toggleExpand = (e: Event) => {
    // Don't expand if clicking a link, button, or inside preview/flags
    const target = e.target as HTMLElement;
    if (target.closest('a') || target.closest('button') || target.closest('.project-card-preview') || target.closest('.project-card-flags')) return;
    const next = !expanded;
    setExpanded(next);
    if (!next) { setActiveTab(null); }
    updateUrl(next, null);
  };

  const selectTab = (tab: CardTab) => {
    const next = activeTab === tab ? null : tab;
    setActiveTab(next);
    // Ensure row 2 is expanded
    if (!expanded) setExpanded(true);
    updateUrl(true, next);
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

  return (
    <div ref={cardRef} class={`project-card-wrapper${expanded ? ' expanded' : ''}${activeTab ? ' has-tab' : ''}`}>
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
              {(['files', 'issues', 'mrs', 'builds', 'deploys', 'sessions'] as CardTab[]).map(t => (
                <button key={t} class={`project-card-tab-btn${activeTab === t ? ' active' : ''}`}
                  onClick={() => selectTab(t)}>
                  {t === 'mrs' ? 'MRs' : t === 'deploys' ? 'Deploys' : t![0].toUpperCase() + t!.slice(1)}
                </button>
              ))}
              <a href={`/projects/${project.id}/settings`} class="project-card-tab-btn" style="margin-left:auto">
                Settings
              </a>
            </div>
          </div>
        </div>
      </div>

      {/* Row 3: Tab content */}
      <div class={`project-card-tab-expand${activeTab ? ' open' : ''}`}>
        <div class="project-card-tab-inner">
          <div class="project-card-tab-content">
            {activeTab === 'files' && <MiniFiles projectId={project.id} defaultBranch={project.default_branch} />}
            {activeTab === 'issues' && <MiniIssues projectId={project.id} />}
            {activeTab === 'mrs' && <MiniMRs projectId={project.id} />}
            {activeTab === 'builds' && <MiniBuilds projectId={project.id} />}
            {activeTab === 'deploys' && <MiniDeploys projectId={project.id} />}
            {activeTab === 'sessions' && <MiniSessions projectId={project.id} />}
            <div class="project-card-tab-footer">
              <a href={`/projects/${project.id}/${activeTab === 'deploys' ? 'deployments' : activeTab}`}>
                Open full {activeTab === 'mrs' ? 'MRs' : activeTab} view
              </a>
            </div>
          </div>
        </div>
      </div>
    </div>
  );
}

/* ---- Mini tab components ---- */

function MiniFiles({ projectId, defaultBranch }: { projectId: string; defaultBranch: string }) {
  const [entries, setEntries] = useState<TreeEntry[]>([]);
  const [branches, setBranches] = useState<BranchInfo[]>([]);
  const [gitRef, setRef] = useState(defaultBranch);
  const [path, setPath] = useState('');
  const [blob, setBlob] = useState<{ path: string; content: string; encoding: string } | null>(null);

  useEffect(() => {
    api.get<BranchInfo[]>(`/api/projects/${projectId}/branches`).then(setBranches).catch(() => {});
  }, [projectId]);

  useEffect(() => {
    setBlob(null);
    api.get<TreeEntry[]>(`/api/projects/${projectId}/tree?ref=${encodeURIComponent(gitRef)}&path=${encodeURIComponent(path)}`)
      .then(setEntries).catch(() => setEntries([]));
  }, [projectId, gitRef, path]);

  const open = (e: TreeEntry) => {
    if (e.entry_type === 'tree') {
      setPath(path ? `${path}/${e.name}` : e.name);
    } else {
      const fp = path ? `${path}/${e.name}` : e.name;
      api.get<{ path: string; content: string; encoding: string }>(`/api/projects/${projectId}/blob?ref=${encodeURIComponent(gitRef)}&path=${encodeURIComponent(fp)}`)
        .then(setBlob).catch(() => {});
    }
  };

  if (blob) {
    return (
      <div>
        <div class="flex-between mb-sm">
          <span class="mono text-xs">{blob.path}</span>
          <button class="btn btn-sm" onClick={() => setBlob(null)}>Back</button>
        </div>
        <pre class="mini-tab-code">{blob.encoding === 'base64' ? atob(blob.content) : blob.content}</pre>
      </div>
    );
  }

  return (
    <div>
      <div class="flex gap-sm mb-sm" style="align-items:center">
        <select class="input" style="width:auto;font-size:0.75rem;padding:0.25rem 0.5rem" value={gitRef}
          onChange={(e) => { setRef((e.target as HTMLSelectElement).value); setPath(''); }}>
          {branches.map(b => <option key={b.name} value={b.name}>{b.name}</option>)}
        </select>
        {path && <button class="btn btn-sm" onClick={() => { const p = path.split('/'); p.pop(); setPath(p.join('/')); }}>..</button>}
        {path && <span class="mono text-xs text-muted">{path}/</span>}
      </div>
      {entries.length === 0 ? <div class="text-muted text-sm">No files</div> : (
        <div class="mini-tab-tree">
          {entries
            .sort((a, b) => a.entry_type === b.entry_type ? a.name.localeCompare(b.name) : a.entry_type === 'tree' ? -1 : 1)
            .map(e => (
              <div key={e.name} class="mini-tab-tree-entry" onClick={() => open(e)}>
                <span class="text-muted">{e.entry_type === 'tree' ? '/' : ' '}</span>
                <span>{e.name}</span>
              </div>
            ))}
        </div>
      )}
    </div>
  );
}

function MiniIssues({ projectId }: { projectId: string }) {
  const [items, setItems] = useState<Issue[] | null>(null);
  const [status, setStatus] = useState('open');

  useEffect(() => {
    setItems(null);
    api.get<ListResponse<Issue>>(`/api/projects/${projectId}/issues?limit=10&status=${status}`)
      .then(r => setItems(r.items)).catch(() => setItems([]));
  }, [projectId, status]);

  return (
    <div>
      <div class="flex gap-sm mb-sm">
        {['open', 'closed'].map(s => (
          <button key={s} class={`btn btn-sm${status === s ? ' btn-primary' : ''}`}
            onClick={() => setStatus(s)}>{s}</button>
        ))}
      </div>
      {items === null ? <div class="text-muted text-sm">Loading...</div> : items.length === 0 ? (
        <div class="text-muted text-sm">No {status} issues</div>
      ) : (
        <table class="table mini-tab-table">
          <thead><tr><th>#</th><th>Title</th><th>Status</th><th>Created</th></tr></thead>
          <tbody>
            {items.map(i => (
              <tr key={i.id} class="table-link" onClick={() => { window.location.href = `/projects/${projectId}/issues/${i.number}`; }}>
                <td class="text-muted">{i.number}</td>
                <td>{i.title}</td>
                <td><Badge status={i.status} /></td>
                <td class="text-muted">{timeAgo(i.created_at)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  );
}

function MiniMRs({ projectId }: { projectId: string }) {
  const [items, setItems] = useState<MergeRequest[] | null>(null);
  const [status, setStatus] = useState('open');

  useEffect(() => {
    setItems(null);
    api.get<ListResponse<MergeRequest>>(`/api/projects/${projectId}/merge-requests?limit=10&status=${status}`)
      .then(r => setItems(r.items)).catch(() => setItems([]));
  }, [projectId, status]);

  return (
    <div>
      <div class="flex gap-sm mb-sm">
        {['open', 'closed', 'merged'].map(s => (
          <button key={s} class={`btn btn-sm${status === s ? ' btn-primary' : ''}`}
            onClick={() => setStatus(s)}>{s}</button>
        ))}
      </div>
      {items === null ? <div class="text-muted text-sm">Loading...</div> : items.length === 0 ? (
        <div class="text-muted text-sm">No {status} merge requests</div>
      ) : (
        <table class="table mini-tab-table">
          <thead><tr><th>#</th><th>Title</th><th>Branches</th><th>Status</th><th>Created</th></tr></thead>
          <tbody>
            {items.map(m => (
              <tr key={m.id} class="table-link" onClick={() => { window.location.href = `/projects/${projectId}/merge-requests/${m.number}`; }}>
                <td class="text-muted">{m.number}</td>
                <td>{m.title}</td>
                <td class="mono text-xs">{m.source_branch} → {m.target_branch}</td>
                <td><Badge status={m.status} /></td>
                <td class="text-muted">{timeAgo(m.created_at)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  );
}

function MiniBuilds({ projectId }: { projectId: string }) {
  const [items, setItems] = useState<Pipeline[] | null>(null);

  useEffect(() => {
    api.get<ListResponse<Pipeline>>(`/api/projects/${projectId}/pipelines?limit=10`)
      .then(r => setItems(r.items)).catch(() => setItems([]));
  }, [projectId]);

  return items === null ? <div class="text-muted text-sm">Loading...</div> : items.length === 0 ? (
    <div class="text-muted text-sm">No builds yet</div>
  ) : (
    <table class="table mini-tab-table">
      <thead><tr><th>Ref</th><th>Trigger</th><th>Status</th><th>Created</th></tr></thead>
      <tbody>
        {items.map(p => (
          <tr key={p.id} class="table-link" onClick={() => { window.location.href = `/projects/${projectId}/pipelines/${p.id}`; }}>
            <td class="mono text-sm">{p.git_ref}</td>
            <td class="text-sm">{p.trigger}</td>
            <td><Badge status={p.status} /></td>
            <td class="text-muted">{timeAgo(p.created_at)}</td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}

function MiniDeploys({ projectId }: { projectId: string }) {
  const [deployments, setDeployments] = useState<Deployment[] | null>(null);

  useEffect(() => {
    api.get<ListResponse<Deployment>>(`/api/projects/${projectId}/deployments?limit=20`)
      .then(r => setDeployments(r.items)).catch(() => setDeployments([]));
  }, [projectId]);

  if (deployments === null) return <div class="text-muted text-sm">Loading...</div>;
  if (deployments.length === 0) return <div class="text-muted text-sm">No deployments yet</div>;

  const envMap = new Map<string, Deployment>();
  for (const d of deployments) {
    if (!envMap.has(d.environment)) envMap.set(d.environment, d);
  }

  return (
    <div class="mini-deploy-grid">
      {Array.from(envMap.entries()).map(([env, d]) => (
        <div key={env} class="mini-deploy-card">
          <div class="mini-deploy-env">{env}</div>
          <div class="flex gap-sm" style="align-items:center">
            <span class="status-dot" style={`background:${d.current_status === 'healthy' || d.current_status === 'running' ? 'var(--success)' : d.current_status === 'failed' ? 'var(--danger)' : 'var(--warning)'}`} />
            <span class="text-sm">{d.current_status}</span>
          </div>
          <div class="mono text-xs text-muted truncate">{d.image_ref}</div>
          <div class="text-xs text-muted">{d.deployed_at ? timeAgo(d.deployed_at) : '--'}</div>
        </div>
      ))}
    </div>
  );
}

function MiniSessions({ projectId }: { projectId: string }) {
  const [items, setItems] = useState<AgentSession[] | null>(null);

  useEffect(() => {
    api.get<ListResponse<AgentSession>>(`/api/projects/${projectId}/sessions?limit=10`)
      .then(r => setItems(r.items)).catch(() => setItems([]));
  }, [projectId]);

  return items === null ? <div class="text-muted text-sm">Loading...</div> : items.length === 0 ? (
    <div class="text-muted text-sm">No sessions yet</div>
  ) : (
    <table class="table mini-tab-table">
      <thead><tr><th>Session</th><th>Status</th><th>Created</th></tr></thead>
      <tbody>
        {items.map(s => (
          <tr key={s.id} class="table-link" onClick={() => { window.location.href = `/projects/${projectId}/sessions/${s.id}`; }}>
            <td class="mono text-sm">{s.id.slice(0, 8)}</td>
            <td><Badge status={s.status} /></td>
            <td class="text-muted">{timeAgo(s.created_at)}</td>
          </tr>
        ))}
      </tbody>
    </table>
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
