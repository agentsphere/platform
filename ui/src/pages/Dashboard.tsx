import { useState, useEffect, useMemo } from 'preact/hooks';
import { api, type ListResponse } from '../lib/api';
import type { Project, DashboardStats } from '../lib/types';
import { useAuth } from '../lib/auth';
import { ProjectCard, type CardTab } from '../components/ProjectCard';
import { SystemHealthPanel } from '../components/SystemHealthPanel';
import { ActivityFeed } from '../components/ActivityFeed';

export function Dashboard() {
  const { user } = useAuth();
  const [projects, setProjects] = useState<Project[]>([]);
  const [total, setTotal] = useState<number | null>(null);
  const [stats, setStats] = useState<DashboardStats | null>(null);

  useEffect(() => {
    api.get<ListResponse<Project>>('/api/projects?limit=20')
      .then(r => { setProjects(r.items); setTotal(r.total); })
      .catch(() => setTotal(0));
    api.get<DashboardStats>('/api/dashboard/stats')
      .then(setStats)
      .catch(e => console.warn(e));
  }, []);

  // Parse URL params for initial expand state
  const urlParams = useMemo(() => {
    const params = new URLSearchParams(window.location.search);
    const validTabs = ['files', 'issues', 'mrs', 'builds', 'deploys', 'sessions'];
    const tab = params.get('tab');
    return {
      projectId: params.get('p'),
      tab: (tab && validTabs.includes(tab) ? tab : null) as CardTab,
    };
  }, []);

  if (total === null) {
    return <div class="empty-state">Loading...</div>;
  }

  const displayName = user?.display_name || user?.name || 'there';

  if (total === 0) {
    return <HeroDashboard displayName={displayName} />;
  }

  return (
    <div class="dashboard-2col">
      {/* Left sidebar */}
      <div class="dashboard-sidebar">
        <SystemHealthPanel />

        <div class="panel">
          <div class="panel-header">Quick Actions</div>
          <div class="panel-body">
            <a href="/create-app" class="quick-action">
              <span>+</span> New Project
            </a>
            <a href="/observe/logs" class="quick-action">
              <span>&#128203;</span> View Logs
            </a>
            <a href="/observe" class="quick-action">
              <span>&#128202;</span> Observability
            </a>
          </div>
        </div>

        {stats && (
          <div class="panel">
            <div class="panel-header">Platform</div>
            <div class="panel-body stats-grid">
              <div class="stat-item">
                <span class="stat-value">{stats.active_sessions}</span>
                <span class="stat-label">Sessions</span>
              </div>
              <div class="stat-item">
                <span class="stat-value">{stats.running_builds}</span>
                <span class="stat-label">Builds</span>
              </div>
              <div class="stat-item">
                <span class="stat-value">{stats.healthy_deployments}</span>
                <span class="stat-label">Healthy</span>
              </div>
              {stats.failed_builds > 0 && (
                <div class="stat-item stat-danger">
                  <span class="stat-value">{stats.failed_builds}</span>
                  <span class="stat-label">Failed</span>
                </div>
              )}
            </div>
          </div>
        )}

        <ActivityFeed />
      </div>

      {/* Center: project cards (vertically centered) */}
      <div class="dashboard-center">
        {projects.map(p => (
          <ProjectCard key={p.id} project={p}
            initialExpanded={urlParams.projectId === p.id}
            initialTab={urlParams.projectId === p.id ? urlParams.tab : null} />
        ))}
        <a href="/create-app" class="project-card-new">
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
            <path d="M12 4v16m8-8H4" />
          </svg>
          New Project
        </a>
      </div>
    </div>
  );
}

function HeroDashboard({ displayName }: { displayName: string }) {
  const [input, setInput] = useState('');

  const go = (prompt: string) => {
    const encoded = encodeURIComponent(prompt);
    window.location.href = `/create-app?prompt=${encoded}`;
  };

  const handleSubmit = (e: Event) => {
    e.preventDefault();
    if (input.trim()) go(input.trim());
  };

  return (
    <div style="position:relative">
      <div class="aurora-bg">
        <div class="aurora-blob-3" />
      </div>
      <div class="hero-container">
        <h1 class="hero-greeting">Hey {displayName}, bring your idea to life</h1>

        <form onSubmit={handleSubmit} style="width:100%;max-width:560px;display:flex;gap:0.5rem">
          <input
            type="text"
            class="hero-chat-input"
            placeholder="Describe what you want to build..."
            value={input}
            onInput={(e) => setInput((e.target as HTMLInputElement).value)}
            autoFocus
          />
          <button type="submit" class="btn btn-primary" style="border-radius:12px;padding:0.9rem 1.5rem" disabled={!input.trim()}>
            Create
          </button>
        </form>

        <div class="hero-options">
          <div class="hero-option-card" onClick={() => go('Import my existing repository from GitHub')}>
            <div class="hero-option-title">Import from GitHub</div>
            <div class="hero-option-desc">Bring an existing repo to the platform</div>
          </div>
          <TemplateOption onSelect={go} />
        </div>
      </div>
    </div>
  );
}

function TemplateOption({ onSelect }: { onSelect: (prompt: string) => void }) {
  const [expanded, setExpanded] = useState(false);

  const templates = [
    { label: 'REST API + Postgres', prompt: 'Create a REST API with Postgres database, auth, and CRUD endpoints' },
    { label: 'Static Site', prompt: 'Create a static website with Markdown content' },
    { label: 'Full-Stack App', prompt: 'Create a full-stack web app with React frontend and API backend' },
  ];

  if (expanded) {
    return (
      <div style="display:flex;flex-direction:column;align-items:center">
        <div class="hero-option-card" onClick={() => setExpanded(false)}>
          <div class="hero-option-title">Start from Template</div>
          <div class="hero-option-desc">Pick a starter to get going fast</div>
        </div>
        <div class="hero-templates">
          {templates.map(t => (
            <button key={t.label} class="hero-template-chip" onClick={() => onSelect(t.prompt)}>
              {t.label}
            </button>
          ))}
        </div>
      </div>
    );
  }

  return (
    <div class="hero-option-card" onClick={() => setExpanded(true)}>
      <div class="hero-option-title">Start from Template</div>
      <div class="hero-option-desc">Pick a starter to get going fast</div>
    </div>
  );
}
