import { useState, useEffect } from 'preact/hooks';
import { api, type ListResponse } from '../lib/api';
import type { Project } from '../lib/types';
import { useAuth } from '../lib/auth';
import { ProjectCard } from '../components/ProjectCard';

interface DashboardStats {
  projects: number;
  active_sessions: number;
  running_builds: number;
  failed_builds: number;
  healthy_deployments: number;
  degraded_deployments: number;
}

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
      .catch(() => {});
  }, []);

  // Still loading
  if (total === null) {
    return <div class="empty-state">Loading...</div>;
  }

  const displayName = user?.display_name || user?.name || 'there';

  // Mode A: No projects — hero screen
  if (total === 0) {
    return <HeroDashboard displayName={displayName} />;
  }

  // Mode B: Has projects — card grid
  return (
    <div>
      <div class="dashboard-grid">
        {/* Subtle stats badge */}
        {stats && stats.active_sessions > 0 && (
          <div style="text-align:right">
            <span class="badge badge-running">{stats.active_sessions} active session{stats.active_sessions !== 1 ? 's' : ''}</span>
          </div>
        )}
        {projects.map(p => (
          <ProjectCard key={p.id} project={p} />
        ))}

        {/* New project card */}
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
