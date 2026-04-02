import { useState, useEffect } from 'preact/hooks';
import { api, qs, type ListResponse } from '../lib/api';
import type {
  Project, Issue, MergeRequest, Pipeline, Webhook,
  TreeEntry, BlobResponse, BranchInfo, Secret,
  LogEntry, UiPreviewArtifact, UiPreviewFile, UiPreviewConfig,
  UiPreviewGroup, UiPreviewItem, AgentSession, IframePanel,
  Comment, PipelineDetail as PipelineDetailType, PipelineStep, Artifact,
} from '../lib/types';
import { timeAgo, duration } from '../lib/format';
import { Badge } from './Badge';
import { StatusDot } from './StatusDot';
import { Pagination } from './Pagination';
import { Modal } from './Modal';
import { Overlay } from './Overlay';
import { FilterBar } from './FilterBar';
import { Markdown } from './Markdown';
import { MermaidBlock, ZoomableDiagram, renderMermaid } from './MermaidBlock';
import { Sessions } from '../pages/Sessions';

/* ---- Helpers ---- */

function fileIcon(name: string, isDir: boolean): string {
  if (isDir) return '\u{1F4C1}';
  const ext = name.split('.').pop()?.toLowerCase() || '';
  const map: Record<string, string> = {
    rs: '\u{1F9E0}', ts: '\u{1F7E6}', tsx: '\u{1F7E6}', js: '\u{1F7E8}', jsx: '\u{1F7E8}',
    json: '{ }', yaml: '\u{2699}', yml: '\u{2699}', toml: '\u{2699}',
    md: '\u{1F4DD}', sql: '\u{1F5C3}', css: '\u{1F3A8}', html: '\u{1F310}',
    sh: '\u{1F4DF}', py: '\u{1F40D}', lock: '\u{1F512}', svg: '\u{1F5BC}',
  };
  return map[ext] || '\u{1F4C4}';
}

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

function langFromPath(path: string): string {
  const ext = path.split('.').pop()?.toLowerCase() || '';
  const map: Record<string, string> = {
    rs: 'rust', ts: 'typescript', tsx: 'tsx', js: 'javascript', jsx: 'jsx',
    json: 'json', yaml: 'yaml', yml: 'yaml', toml: 'toml', md: 'markdown',
    sql: 'sql', css: 'css', html: 'html', sh: 'bash', py: 'python',
    xml: 'xml', svg: 'xml',
  };
  return map[ext] || 'text';
}

type RepoSource = 'app' | 'ops';

interface TreeNode {
  entry: TreeEntry;
  fullPath: string;
  children: TreeNode[] | null;
  expanded: boolean;
}

function sortEntries(entries: TreeEntry[]): TreeEntry[] {
  return [...entries].sort((a, b) =>
    a.entry_type === b.entry_type ? a.name.localeCompare(b.name) : a.entry_type === 'tree' ? -1 : 1
  );
}

function TreeRow({ node, depth, activePath, onToggle, onSelect }: {
  node: TreeNode;
  depth: number;
  activePath: string | null;
  onToggle: (path: string) => void;
  onSelect: (path: string) => void;
}) {
  const isDir = node.entry.entry_type === 'tree';
  const isActive = activePath === node.fullPath;
  return (
    <>
      <div
        class={`repo-tree-row${isActive ? ' active' : ''}`}
        style={`padding-left: ${0.75 + depth * 1}rem`}
        onClick={() => isDir ? onToggle(node.fullPath) : onSelect(node.fullPath)}
      >
        {isDir && (
          <span class={`repo-tree-chevron${node.expanded ? ' open' : ''}`}>
            {'\u25B6'}
          </span>
        )}
        {!isDir && <span class="repo-tree-chevron-spacer" />}
        <span class="repo-tree-icon">{fileIcon(node.entry.name, isDir)}</span>
        <span class="repo-tree-name">{node.entry.name}</span>
      </div>
      {isDir && node.expanded && node.children && node.children.map(child => (
        <TreeRow
          key={child.fullPath}
          node={child}
          depth={depth + 1}
          activePath={activePath}
          onToggle={onToggle}
          onSelect={onSelect}
        />
      ))}
      {isDir && node.expanded && node.children && node.children.length === 0 && (
        <div class="repo-tree-row text-muted" style={`padding-left: ${0.75 + (depth + 1) * 1}rem; font-style: italic; font-size: 0.78rem`}>
          (empty)
        </div>
      )}
    </>
  );
}

/* ---- Files Tab ---- */

export function FilesTab({ projectId, defaultBranch }: { projectId: string; defaultBranch: string }) {
  const [repo, setRepo] = useState<RepoSource>('app');
  const [branches, setBranches] = useState<BranchInfo[]>([]);
  const [opsBranches, setOpsBranches] = useState<BranchInfo[]>([]);
  const [gitRef, setRef] = useState(defaultBranch);
  const [opsRef, setOpsRef] = useState('main');
  const [tree, setTree] = useState<TreeNode[]>([]);
  const [blob, setBlob] = useState<BlobResponse | null>(null);
  const [loading, setLoading] = useState(false);
  const [copied, setCopied] = useState(false);
  const [hasOps, setHasOps] = useState(false);

  const currentRef = repo === 'app' ? gitRef : opsRef;
  const apiPrefix = repo === 'app'
    ? `/api/projects/${projectId}`
    : `/api/projects/${projectId}/ops-repo`;

  useEffect(() => {
    api.get<BranchInfo[]>(`/api/projects/${projectId}/branches`).then(setBranches).catch(() => {});
    api.get<BranchInfo[]>(`/api/projects/${projectId}/ops-repo/branches`)
      .then(b => { setOpsBranches(b); setHasOps(true); if (b.length > 0 && !b.find(br => br.name === opsRef)) setOpsRef(b[0].name); })
      .catch(() => setHasOps(false));
  }, [projectId]);

  useEffect(() => {
    setBlob(null);
    setTree([]);
    api.get<TreeEntry[]>(`${apiPrefix}/tree${qs({ ref: currentRef, path: '' })}`)
      .then(entries => {
        setTree(sortEntries(entries).map(e => ({
          entry: e, fullPath: e.name, children: null, expanded: false,
        })));
      })
      .catch(() => setTree([]));
  }, [projectId, repo, currentRef]);

  const loadChildren = async (dirPath: string): Promise<TreeNode[]> => {
    const entries = await api.get<TreeEntry[]>(`${apiPrefix}/tree${qs({ ref: currentRef, path: dirPath })}`);
    return sortEntries(entries).map(e => ({
      entry: e,
      fullPath: dirPath ? `${dirPath}/${e.name}` : e.name,
      children: null, expanded: false,
    }));
  };

  const updateNode = (nodes: TreeNode[], targetPath: string, updater: (n: TreeNode) => TreeNode): TreeNode[] => {
    return nodes.map(n => {
      if (n.fullPath === targetPath) return updater(n);
      if (targetPath.startsWith(n.fullPath + '/') && n.children) {
        return { ...n, children: updateNode(n.children, targetPath, updater) };
      }
      return n;
    });
  };

  const toggleDir = async (dirPath: string) => {
    const findNode = (nodes: TreeNode[], path: string): TreeNode | null => {
      for (const n of nodes) {
        if (n.fullPath === path) return n;
        if (path.startsWith(n.fullPath + '/') && n.children) {
          const found = findNode(n.children, path);
          if (found) return found;
        }
      }
      return null;
    };
    const node = findNode(tree, dirPath);
    if (!node) return;
    if (node.expanded) {
      setTree(prev => updateNode(prev, dirPath, n => ({ ...n, expanded: false })));
    } else {
      if (node.children === null) {
        const children = await loadChildren(dirPath);
        setTree(prev => updateNode(prev, dirPath, n => ({ ...n, children, expanded: true })));
      } else {
        setTree(prev => updateNode(prev, dirPath, n => ({ ...n, expanded: true })));
      }
    }
  };

  const selectFile = (filePath: string) => {
    setLoading(true);
    api.get<BlobResponse>(`${apiPrefix}/blob${qs({ ref: currentRef, path: filePath })}`)
      .then(setBlob).catch(e => console.warn(e)).finally(() => setLoading(false));
  };

  const copyFile = () => {
    if (!blob) return;
    const text = blob.encoding === 'base64' ? atob(blob.content) : blob.content;
    navigator.clipboard.writeText(text).then(() => {
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    });
  };

  const blobContent = blob ? (blob.encoding === 'base64' ? atob(blob.content) : blob.content) : '';
  const blobLines = blob ? blobContent.split('\n') : [];
  const activeBranches = repo === 'app' ? branches : opsBranches;

  return (
    <div class="repo-browser">
      <div class="repo-toolbar">
        <div class="repo-switcher">
          <button class={`repo-switcher-btn${repo === 'app' ? ' active' : ''}`}
            onClick={() => { setRepo('app'); setBlob(null); }}>Source</button>
          {hasOps && (
            <button class={`repo-switcher-btn${repo === 'ops' ? ' active' : ''}`}
              onClick={() => { setRepo('ops'); setBlob(null); }}>Ops</button>
          )}
        </div>
        <select class="input repo-branch-select" value={currentRef}
          onChange={(e) => {
            const v = (e.target as HTMLSelectElement).value;
            if (repo === 'app') setRef(v); else setOpsRef(v);
            setBlob(null);
          }}>
          {activeBranches.map(b => <option key={b.name} value={b.name}>{b.name}</option>)}
        </select>
        {blob && (
          <div class="repo-breadcrumb">
            <span class="repo-breadcrumb-file">{blob.path}</span>
          </div>
        )}
      </div>
      <div class="repo-columns">
        <div class="repo-tree-panel">
          {tree.length === 0 ? (
            <div class="repo-tree-empty">No files</div>
          ) : (
            tree.map(node => (
              <TreeRow key={node.fullPath} node={node} depth={0}
                activePath={blob?.path || null} onToggle={toggleDir} onSelect={selectFile} />
            ))
          )}
        </div>
        <div class="repo-content-panel">
          {loading && <div class="repo-content-loading">Loading...</div>}
          {!blob && !loading && (
            <div class="repo-content-empty">
              <span class="repo-content-empty-icon">{'\u{1F4C2}'}</span>
              <p>Select a file to view its contents</p>
            </div>
          )}
          {blob && !loading && (
            <>
              <div class="repo-content-header">
                <span class="repo-content-filename">{blob.path.split('/').pop()}</span>
                <div class="repo-content-meta">
                  <span class="text-muted text-xs">{formatBytes(blob.size)}</span>
                  <span class="text-muted text-xs">{blobLines.length} lines</span>
                  <span class="text-muted text-xs">{langFromPath(blob.path)}</span>
                  <button class="btn btn-ghost btn-xs" onClick={copyFile}>
                    {copied ? 'Copied' : 'Copy'}
                  </button>
                </div>
              </div>
              <div class="repo-content-code">
                <table class="repo-code-table">
                  <tbody>
                    {blobLines.map((line, i) => (
                      <tr key={i} class="repo-code-row">
                        <td class="repo-line-no">{i + 1}</td>
                        <td class="repo-line-code"><pre>{line}</pre></td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            </>
          )}
        </div>
      </div>
    </div>
  );
}

/* ---- Issues Tab ---- */

export function IssuesTab({ projectId }: { projectId: string }) {
  const [issues, setIssues] = useState<Issue[]>([]);
  const [total, setTotal] = useState(0);
  const [offset, setOffset] = useState(0);
  const [status, setStatus] = useState('open');
  const [showCreate, setShowCreate] = useState(false);
  const [form, setForm] = useState({ title: '', body: '' });
  const [error, setError] = useState('');
  const [selectedIssue, setSelectedIssue] = useState<Issue | null>(null);
  const [comments, setComments] = useState<Comment[]>([]);
  const [commentBody, setCommentBody] = useState('');
  const [commentError, setCommentError] = useState('');

  const load = () => {
    api.get<ListResponse<Issue>>(`/api/projects/${projectId}/issues${qs({ limit: 20, offset, status })}`)
      .then(r => { setIssues(r.items); setTotal(r.total); }).catch(e => console.warn(e));
  };
  useEffect(load, [projectId, offset, status]);

  const openIssue = (issue: Issue) => {
    setSelectedIssue(issue);
    setComments([]);
    setCommentBody('');
    api.get<ListResponse<Comment>>(`/api/projects/${projectId}/issues/${issue.number}/comments?limit=100`)
      .then(r => setComments(r.items)).catch(() => {});
  };

  const toggleIssueStatus = async () => {
    if (!selectedIssue) return;
    const newStatus = selectedIssue.status === 'open' ? 'closed' : 'open';
    const updated = await api.patch<Issue>(`/api/projects/${projectId}/issues/${selectedIssue.number}`, { status: newStatus });
    setSelectedIssue(updated);
    load();
  };

  const addComment = async (e: Event) => {
    e.preventDefault();
    if (!commentBody.trim() || !selectedIssue) return;
    try {
      await api.post(`/api/projects/${projectId}/issues/${selectedIssue.number}/comments`, { body: commentBody });
      setCommentBody('');
      setCommentError('');
      api.get<ListResponse<Comment>>(`/api/projects/${projectId}/issues/${selectedIssue.number}/comments?limit=100`)
        .then(r => setComments(r.items)).catch(() => {});
    } catch (err: any) { setCommentError(err.message); }
  };

  const create = async (e: Event) => {
    e.preventDefault();
    try {
      await api.post(`/api/projects/${projectId}/issues`, form);
      setShowCreate(false);
      setForm({ title: '', body: '' });
      load();
    } catch (err: any) { setError(err.message); }
  };

  return (
    <div>
      <div class="flex-between mb-md">
        <div class="flex gap-sm">
          {['open', 'closed'].map(s => (
            <button key={s} class={`btn btn-sm${status === s ? ' btn-primary' : ''}`}
              onClick={() => { setStatus(s); setOffset(0); }}>{s}</button>
          ))}
        </div>
        <button class="btn btn-primary btn-sm" onClick={() => setShowCreate(true)}>New Issue</button>
      </div>
      <div class="card">
        {issues.length === 0 ? <div class="empty-state">No issues</div> : (
          <table class="table">
            <thead><tr><th>#</th><th>Title</th><th>Status</th><th>Created</th></tr></thead>
            <tbody>
              {issues.map(i => (
                <tr key={i.id} class="table-link" onClick={() => openIssue(i)}>
                  <td class="text-muted">{i.number}</td>
                  <td>{i.title}</td>
                  <td><Badge status={i.status} /></td>
                  <td class="text-muted text-sm">{timeAgo(i.created_at)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
        <Pagination total={total} limit={20} offset={offset} onChange={setOffset} />
      </div>

      {/* Issue detail overlay */}
      <Overlay open={!!selectedIssue} onClose={() => setSelectedIssue(null)}
        title={selectedIssue ? `#${selectedIssue.number} ${selectedIssue.title}` : ''}>
        {selectedIssue && (
          <div>
            <div class="flex-between mb-md">
              <div class="flex gap-sm">
                <Badge status={selectedIssue.status} />
                {selectedIssue.labels && selectedIssue.labels.length > 0 && (
                  <div class="flex gap-sm">{selectedIssue.labels.map(l => <span key={l} class="label-tag">{l}</span>)}</div>
                )}
              </div>
              <button class="btn btn-sm" onClick={toggleIssueStatus}>
                {selectedIssue.status === 'open' ? 'Close Issue' : 'Reopen'}
              </button>
            </div>

            {selectedIssue.body && (
              <div class="card mb-md">
                <Markdown content={selectedIssue.body} />
              </div>
            )}

            <h3 style="font-size:0.9rem" class="mb-md">Comments ({comments.length})</h3>
            {comments.map(c => (
              <div key={c.id} class="comment-box">
                <div class="comment-header">commented {timeAgo(c.created_at)}</div>
                <Markdown content={c.body} />
              </div>
            ))}

            <div class="card mt-md">
              <form onSubmit={addComment}>
                <div class="form-group">
                  <label>Add a comment</label>
                  <textarea class="input" value={commentBody} rows={3}
                    onInput={(e) => setCommentBody((e.target as HTMLTextAreaElement).value)} />
                </div>
                {commentError && <div class="error-msg">{commentError}</div>}
                <button type="submit" class="btn btn-primary btn-sm">Comment</button>
              </form>
            </div>
          </div>
        )}
      </Overlay>

      <Modal open={showCreate} onClose={() => setShowCreate(false)} title="New Issue">
        <form onSubmit={create}>
          <div class="form-group">
            <label>Title</label>
            <input class="input" required value={form.title}
              onInput={(e) => setForm({ ...form, title: (e.target as HTMLInputElement).value })} />
          </div>
          <div class="form-group">
            <label>Body (markdown)</label>
            <textarea class="input" value={form.body}
              onInput={(e) => setForm({ ...form, body: (e.target as HTMLTextAreaElement).value })} />
          </div>
          {error && <div class="error-msg">{error}</div>}
          <div class="modal-actions">
            <button type="button" class="btn" onClick={() => setShowCreate(false)}>Cancel</button>
            <button type="submit" class="btn btn-primary">Create</button>
          </div>
        </form>
      </Modal>
    </div>
  );
}

/* ---- MRs Tab (with pipeline status) ---- */

export function MRsTab({ projectId }: { projectId: string }) {
  const [mrs, setMrs] = useState<MergeRequest[]>([]);
  const [total, setTotal] = useState(0);
  const [offset, setOffset] = useState(0);
  const [status, setStatus] = useState('open');
  const [showCreate, setShowCreate] = useState(false);
  const [branches, setBranches] = useState<BranchInfo[]>([]);
  const [form, setForm] = useState({ source_branch: '', target_branch: 'main', title: '', body: '' });
  const [error, setError] = useState('');
  const [branchPipelines, setBranchPipelines] = useState<Map<string, Pipeline>>(new Map());

  const load = () => {
    api.get<ListResponse<MergeRequest>>(`/api/projects/${projectId}/merge-requests${qs({ limit: 20, offset, status })}`)
      .then(r => { setMrs(r.items); setTotal(r.total); }).catch(e => console.warn(e));
  };
  useEffect(load, [projectId, offset, status]);

  // Fetch latest pipeline for each MR's source branch
  useEffect(() => {
    if (mrs.length === 0) return;
    const uniqueBranches = [...new Set(mrs.map(m => m.source_branch))];
    const pipelineMap = new Map<string, Pipeline>();
    Promise.all(
      uniqueBranches.map(branch =>
        api.get<ListResponse<Pipeline>>(`/api/projects/${projectId}/pipelines${qs({ limit: 1, git_ref: branch })}`)
          .then(r => { if (r.items.length > 0) pipelineMap.set(branch, r.items[0]); })
          .catch(() => {})
      )
    ).then(() => setBranchPipelines(new Map(pipelineMap)));
  }, [mrs, projectId]);

  const openCreate = () => {
    api.get<BranchInfo[]>(`/api/projects/${projectId}/branches`).then(setBranches).catch(e => console.warn(e));
    setShowCreate(true);
  };

  const create = async (e: Event) => {
    e.preventDefault();
    try {
      await api.post(`/api/projects/${projectId}/merge-requests`, form);
      setShowCreate(false);
      setForm({ source_branch: '', target_branch: 'main', title: '', body: '' });
      load();
    } catch (err: any) { setError(err.message); }
  };

  const pipelineIcon = (status: string) => {
    if (status === 'success') return '\u2713';
    if (status === 'running' || status === 'pending') return '\u21BB';
    if (status === 'failure' || status === 'cancelled') return '\u2717';
    return '\u00B7';
  };

  const statusColor = (s: string): string => {
    if (s === 'success') return 'var(--success)';
    if (s === 'running' || s === 'pending') return 'var(--warning)';
    if (s === 'failure' || s === 'cancelled') return 'var(--danger)';
    return 'var(--text-muted)';
  };

  return (
    <div>
      <div class="flex-between mb-md">
        <div class="flex gap-sm">
          {['open', 'closed', 'merged'].map(s => (
            <button key={s} class={`btn btn-sm${status === s ? ' btn-primary' : ''}`}
              onClick={() => { setStatus(s); setOffset(0); }}>{s}</button>
          ))}
        </div>
        <button class="btn btn-primary btn-sm" onClick={openCreate}>New MR</button>
      </div>
      <div class="card">
        {mrs.length === 0 ? <div class="empty-state">No merge requests</div> : (
          <table class="table">
            <thead><tr><th>#</th><th>Title</th><th>Branches</th><th>Build</th><th>Status</th><th>Created</th></tr></thead>
            <tbody>
              {mrs.map(m => {
                const pl = branchPipelines.get(m.source_branch);
                return (
                  <tr key={m.id} class="table-link" onClick={() => { window.location.href = `/projects/${projectId}/merge-requests/${m.number}`; }}>
                    <td class="text-muted">{m.number}</td>
                    <td>{m.title}</td>
                    <td class="mono text-xs">{m.source_branch} → {m.target_branch}</td>
                    <td>
                      {pl ? (
                        <span style={`color:${statusColor(pl.status)};font-weight:600`} title={`Build: ${pl.status}`}>
                          {pipelineIcon(pl.status)} {pl.status}
                        </span>
                      ) : (
                        <span class="text-muted text-xs">--</span>
                      )}
                    </td>
                    <td><Badge status={m.status} /></td>
                    <td class="text-muted text-sm">{timeAgo(m.created_at)}</td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        )}
        <Pagination total={total} limit={20} offset={offset} onChange={setOffset} />
      </div>
      <Modal open={showCreate} onClose={() => setShowCreate(false)} title="New Merge Request">
        <form onSubmit={create}>
          <div class="form-group">
            <label>Source branch</label>
            <select class="input" value={form.source_branch}
              onChange={(e) => setForm({ ...form, source_branch: (e.target as HTMLSelectElement).value })}>
              <option value="">Select...</option>
              {branches.map(b => <option key={b.name} value={b.name}>{b.name}</option>)}
            </select>
          </div>
          <div class="form-group">
            <label>Target branch</label>
            <select class="input" value={form.target_branch}
              onChange={(e) => setForm({ ...form, target_branch: (e.target as HTMLSelectElement).value })}>
              {branches.map(b => <option key={b.name} value={b.name}>{b.name}</option>)}
            </select>
          </div>
          <div class="form-group">
            <label>Title</label>
            <input class="input" required value={form.title}
              onInput={(e) => setForm({ ...form, title: (e.target as HTMLInputElement).value })} />
          </div>
          <div class="form-group">
            <label>Description</label>
            <textarea class="input" value={form.body}
              onInput={(e) => setForm({ ...form, body: (e.target as HTMLTextAreaElement).value })} />
          </div>
          {error && <div class="error-msg">{error}</div>}
          <div class="modal-actions">
            <button type="button" class="btn" onClick={() => setShowCreate(false)}>Cancel</button>
            <button type="submit" class="btn btn-primary">Create</button>
          </div>
        </form>
      </Modal>
    </div>
  );
}

/* ---- Builds Tab ---- */

export function BuildsTab({ projectId }: { projectId: string }) {
  const [pipelines, setPipelines] = useState<Pipeline[]>([]);
  const [total, setTotal] = useState(0);
  const [offset, setOffset] = useState(0);
  const [selectedPipeline, setSelectedPipeline] = useState<PipelineDetailType | null>(null);
  const [artifacts, setArtifacts] = useState<Artifact[]>([]);
  const [selectedStep, setSelectedStep] = useState<PipelineStep | null>(null);
  const [stepLogs, setStepLogs] = useState('');

  const load = () => {
    api.get<ListResponse<Pipeline>>(`/api/projects/${projectId}/pipelines${qs({ limit: 20, offset })}`)
      .then(r => { setPipelines(r.items); setTotal(r.total); }).catch(e => console.warn(e));
  };
  useEffect(load, [projectId, offset]);

  const openPipeline = (p: Pipeline) => {
    setSelectedStep(null);
    setStepLogs('');
    setArtifacts([]);
    api.get<PipelineDetailType>(`/api/projects/${projectId}/pipelines/${p.id}`)
      .then(setSelectedPipeline).catch(() => {});
    api.get<Artifact[]>(`/api/projects/${projectId}/pipelines/${p.id}/artifacts`)
      .then(setArtifacts).catch(() => setArtifacts([]));
  };

  const viewLogs = async (step: PipelineStep) => {
    if (!selectedPipeline) return;
    setSelectedStep(step);
    setStepLogs('Loading...');
    try {
      const res = await fetch(`/api/projects/${projectId}/pipelines/${selectedPipeline.id}/steps/${step.id}/logs`, { credentials: 'include' });
      setStepLogs(await res.text());
    } catch { setStepLogs('Failed to load logs'); }
  };

  const cancelPipeline = async () => {
    if (!selectedPipeline) return;
    try {
      await api.post(`/api/projects/${projectId}/pipelines/${selectedPipeline.id}/cancel`);
      const updated = await api.get<PipelineDetailType>(`/api/projects/${projectId}/pipelines/${selectedPipeline.id}`);
      setSelectedPipeline(updated);
      load();
    } catch { /* ignore */ }
  };

  return (
    <div>
      <div class="card">
        {pipelines.length === 0 ? <div class="empty-state">No pipelines</div> : (
          <table class="table">
            <thead><tr><th>Ref</th><th>Trigger</th><th>Status</th><th>Created</th></tr></thead>
            <tbody>
              {pipelines.map(p => (
                <tr key={p.id} class="table-link" onClick={() => openPipeline(p)}>
                  <td class="mono text-sm">{p.git_ref}</td>
                  <td class="text-sm">{p.trigger}</td>
                  <td><Badge status={p.status} /></td>
                  <td class="text-muted text-sm">{timeAgo(p.created_at)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
        <Pagination total={total} limit={20} offset={offset} onChange={setOffset} />
      </div>

      {/* Pipeline detail overlay */}
      <Overlay open={!!selectedPipeline} onClose={() => { setSelectedPipeline(null); setSelectedStep(null); }}
        title={selectedPipeline ? `Pipeline: ${selectedPipeline.git_ref}` : ''}>
        {selectedPipeline && (
          <div>
            <div class="flex-between mb-md">
              <div class="flex gap-sm" style="align-items:center">
                <Badge status={selectedPipeline.status} />
                <span class="text-sm text-muted">
                  {selectedPipeline.trigger}
                  {selectedPipeline.commit_sha && <> · <span class="mono">{selectedPipeline.commit_sha.substring(0, 8)}</span></>}
                  {' · '}{timeAgo(selectedPipeline.created_at)}
                  {selectedPipeline.started_at && selectedPipeline.finished_at && (
                    <> · {duration(new Date(selectedPipeline.finished_at).getTime() - new Date(selectedPipeline.started_at).getTime())}</>
                  )}
                </span>
              </div>
              {(selectedPipeline.status === 'pending' || selectedPipeline.status === 'running') && (
                <button class="btn btn-danger btn-sm" onClick={cancelPipeline}>Cancel</button>
              )}
            </div>

            <h3 style="font-size:0.9rem" class="mb-md">Steps</h3>
            <div class="card mb-md">
              <table class="table">
                <thead><tr><th>#</th><th>Name</th><th>Image</th><th>Status</th><th>Duration</th><th></th></tr></thead>
                <tbody>
                  {(selectedPipeline.steps || []).map(s => (
                    <tr key={s.id}>
                      <td class="text-muted">{s.step_order}</td>
                      <td>
                        {s.name}
                        {s.gate && <span class="badge badge-info ml-xs" title="Quality Gate">Gate</span>}
                        {s.depends_on && s.depends_on.length > 0 && (
                          <span class="text-xs text-muted ml-xs">({s.depends_on.join(', ')})</span>
                        )}
                      </td>
                      <td class="mono text-xs">{s.image}</td>
                      <td><Badge status={s.status} /> {s.exit_code != null && <span class="text-xs text-muted">exit {s.exit_code}</span>}</td>
                      <td class="text-sm">{s.duration_ms != null ? duration(s.duration_ms) : '\u2014'}</td>
                      <td><button class="btn btn-ghost btn-sm" onClick={() => viewLogs(s)}>Logs</button></td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>

            {selectedStep && (
              <div class="mb-md">
                <div class="flex-between mb-md">
                  <h3 style="font-size:0.9rem">Logs: {selectedStep.name}</h3>
                  <button class="btn btn-sm" onClick={() => { setSelectedStep(null); setStepLogs(''); }}>Close</button>
                </div>
                <div class="log-viewer">{stepLogs}</div>
              </div>
            )}

            {artifacts.length > 0 && (
              <div>
                <h3 style="font-size:0.9rem" class="mb-md">Artifacts</h3>
                <div class="card">
                  <table class="table">
                    <thead><tr><th>Name</th><th>Type</th><th>Size</th><th></th></tr></thead>
                    <tbody>
                      {artifacts.map(a => (
                        <tr key={a.id}>
                          <td>{a.name}</td>
                          <td class="text-xs text-muted">{a.content_type || '\u2014'}</td>
                          <td class="text-sm">{a.size_bytes != null ? `${a.size_bytes} B` : '\u2014'}</td>
                          <td>
                            <a class="btn btn-sm" href={`/api/projects/${projectId}/pipelines/${selectedPipeline.id}/artifacts/${a.id}/download`}>
                              Download
                            </a>
                          </td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </div>
              </div>
            )}
          </div>
        )}
      </Overlay>
    </div>
  );
}

/* ---- UI Previews Tab ---- */

interface UiPreviewsCompareResponse {
  base: UiPreviewArtifact[];
  head: UiPreviewArtifact[];
}

export function UiPreviewsTab({ projectId, defaultBranch }: { projectId: string; defaultBranch: string }) {
  const [branches, setBranches] = useState<BranchInfo[]>([]);
  const [branch, setBranch] = useState(defaultBranch);
  const [typeFilter, setTypeFilter] = useState<string>('all');
  const [artifacts, setArtifacts] = useState<UiPreviewArtifact[]>([]);
  const [loading, setLoading] = useState(true);
  const [collapsed, setCollapsed] = useState<Set<string>>(new Set());
  const [metaFilter, setMetaFilter] = useState<{ key: string; value: string } | null>(null);
  const [lightbox, setLightbox] = useState<{ file: UiPreviewFile; artifact: UiPreviewArtifact; item: UiPreviewItem | null } | null>(null);
  const [compareEnabled, setCompareEnabled] = useState(false);
  const [compareBranch, setCompareBranch] = useState('');
  const [compareData, setCompareData] = useState<UiPreviewsCompareResponse | null>(null);

  useEffect(() => {
    api.get<BranchInfo[]>(`/api/projects/${projectId}/branches`).then(setBranches).catch(() => {});
  }, [projectId]);

  useEffect(() => {
    setLoading(true);
    const typeParam = typeFilter === 'all' ? '' : typeFilter;
    api.get<UiPreviewArtifact[]>(`/api/projects/${projectId}/ui-previews${qs({ branch, type: typeParam })}`)
      .then(setArtifacts).catch(() => setArtifacts([])).finally(() => setLoading(false));
  }, [projectId, branch, typeFilter]);

  useEffect(() => {
    if (!compareEnabled || !compareBranch) { setCompareData(null); return; }
    api.get<UiPreviewsCompareResponse>(`/api/projects/${projectId}/ui-previews/compare${qs({ base: branch, head: compareBranch })}`)
      .then(setCompareData).catch(() => setCompareData(null));
  }, [projectId, branch, compareBranch, compareEnabled]);

  const toggleCollapsed = (key: string) => {
    setCollapsed(prev => { const next = new Set(prev); if (next.has(key)) next.delete(key); else next.add(key); return next; });
  };
  const toggleMetaFilter = (key: string, value: string) => {
    if (metaFilter && metaFilter.key === key && metaFilter.value === value) setMetaFilter(null);
    else setMetaFilter({ key, value });
  };
  const imageUrl = (pipelineId: string, fileId: string) =>
    `/api/projects/${projectId}/pipelines/${pipelineId}/artifacts/${fileId}/view`;

  const findItemForFile = (artifact: UiPreviewArtifact, file: UiPreviewFile): UiPreviewItem | null => {
    if (!artifact.config) return null;
    const search = (groups: Record<string, UiPreviewGroup>): UiPreviewItem | null => {
      for (const g of Object.values(groups)) {
        if (g.items) { for (const [filename, item] of Object.entries(g.items)) { if (file.relative_path.endsWith(filename)) return item; } }
        if (g.groups) { const found = search(g.groups); if (found) return found; }
      }
      return null;
    };
    return search(artifact.config.groups);
  };

  const passesMetaFilter = (artifact: UiPreviewArtifact, file: UiPreviewFile): boolean => {
    if (!metaFilter) return true;
    const item = findItemForFile(artifact, file);
    if (!item || !item.meta) return false;
    return item.meta[metaFilter.key] === metaFilter.value;
  };

  const allMeta = new Map<string, Set<string>>();
  for (const a of artifacts) {
    if (!a.config) continue;
    const collectMeta = (groups: Record<string, UiPreviewGroup>) => {
      for (const g of Object.values(groups)) {
        if (g.items) { for (const item of Object.values(g.items)) { if (item.meta) { for (const [k, v] of Object.entries(item.meta)) { if (!allMeta.has(k)) allMeta.set(k, new Set()); allMeta.get(k)!.add(v); } } } }
        if (g.groups) collectMeta(g.groups);
      }
    };
    collectMeta(a.config.groups);
  }

  const renderGroup = (key: string, group: UiPreviewGroup, artifact: UiPreviewArtifact, path: string, depth: number) => {
    const fullKey = path ? `${path}.${key}` : key;
    const isCollapsed = collapsed.has(fullKey);
    const items: { file: UiPreviewFile; item: UiPreviewItem | null; filename: string }[] = [];
    if (group.items) {
      for (const [filename, item] of Object.entries(group.items)) {
        const file = artifact.files.find(f => f.relative_path.endsWith(filename));
        if (file && passesMetaFilter(artifact, file)) items.push({ file, item, filename });
      }
    }
    const hasSubGroups = group.groups && Object.keys(group.groups).length > 0;
    const hasItems = items.length > 0;
    if (!hasSubGroups && !hasItems && group.items && Object.keys(group.items).length > 0) return null;

    return (
      <div key={fullKey} class="ui-preview-group">
        <div class="ui-preview-group-header" onClick={() => toggleCollapsed(fullKey)}>
          <span class="toggle-icon">{isCollapsed ? '\u25B8' : '\u25BE'}</span>
          {group.label}
        </div>
        {!isCollapsed && (
          <div class="ui-preview-group-children">
            {group.groups && Object.entries(group.groups).map(([k, g]) => renderGroup(k, g, artifact, fullKey, depth + 1))}
            {hasItems && (
              <div class="ui-preview-grid">
                {items.map(({ file, item }) => (
                  <div key={file.id} class="ui-preview-card" onClick={() => setLightbox({ file, artifact, item })}>
                    <img src={imageUrl(artifact.pipeline_id, file.id)} alt={item?.label || file.relative_path} loading="lazy" />
                    <div class="ui-preview-card-label">{item?.label || file.relative_path.split('/').pop()}</div>
                    {item?.meta && (
                      <div style="padding:0 0.5rem 0.4rem">
                        {Object.entries(item.meta).map(([mk, mv]) => (
                          <span key={`${mk}-${mv}`}
                            class={`ui-preview-meta-badge${metaFilter && metaFilter.key === mk && metaFilter.value === mv ? ' active' : ''}`}
                            onClick={(e) => { e.stopPropagation(); toggleMetaFilter(mk, mv); }}>{mv}</span>
                        ))}
                      </div>
                    )}
                  </div>
                ))}
              </div>
            )}
          </div>
        )}
      </div>
    );
  };

  const renderUncategorized = (artifact: UiPreviewArtifact) => {
    if (!artifact.config) {
      const filtered = artifact.files.filter(f => passesMetaFilter(artifact, f));
      if (filtered.length === 0) return null;
      return (
        <div class="ui-preview-grid">
          {filtered.map(file => (
            <div key={file.id} class="ui-preview-card" onClick={() => setLightbox({ file, artifact, item: null })}>
              <img src={imageUrl(artifact.pipeline_id, file.id)} alt={file.relative_path} loading="lazy" />
              <div class="ui-preview-card-label">{file.relative_path.split('/').pop()}</div>
            </div>
          ))}
        </div>
      );
    }
    const referencedFiles = new Set<string>();
    const collectRefs = (groups: Record<string, UiPreviewGroup>) => {
      for (const g of Object.values(groups)) {
        if (g.items) { for (const filename of Object.keys(g.items)) { for (const f of artifact.files) { if (f.relative_path.endsWith(filename)) referencedFiles.add(f.id); } } }
        if (g.groups) collectRefs(g.groups);
      }
    };
    collectRefs(artifact.config.groups);
    const uncategorized = artifact.files.filter(f => !referencedFiles.has(f.id) && passesMetaFilter(artifact, f));
    if (uncategorized.length === 0) return null;
    const isCollapsed = collapsed.has('__uncategorized');
    return (
      <div class="ui-preview-group">
        <div class="ui-preview-group-header" onClick={() => toggleCollapsed('__uncategorized')}>
          <span class="toggle-icon">{isCollapsed ? '\u25B8' : '\u25BE'}</span>Uncategorized
        </div>
        {!isCollapsed && (
          <div class="ui-preview-group-children">
            <div class="ui-preview-grid">
              {uncategorized.map(file => (
                <div key={file.id} class="ui-preview-card" onClick={() => setLightbox({ file, artifact, item: null })}>
                  <img src={imageUrl(artifact.pipeline_id, file.id)} alt={file.relative_path} loading="lazy" />
                  <div class="ui-preview-card-label">{file.relative_path.split('/').pop()}</div>
                </div>
              ))}
            </div>
          </div>
        )}
      </div>
    );
  };

  const renderCompare = () => {
    if (!compareData) return <div class="empty-state">Loading comparison...</div>;
    const baseFiles = new Map<string, { file: UiPreviewFile; artifact: UiPreviewArtifact }>();
    const headFiles = new Map<string, { file: UiPreviewFile; artifact: UiPreviewArtifact }>();
    for (const a of compareData.base) { for (const f of a.files) baseFiles.set(f.relative_path, { file: f, artifact: a }); }
    for (const a of compareData.head) { for (const f of a.files) headFiles.set(f.relative_path, { file: f, artifact: a }); }
    const allPaths = new Set([...baseFiles.keys(), ...headFiles.keys()]);
    if (allPaths.size === 0) return <div class="empty-state">No files to compare</div>;
    return (
      <div class="ui-preview-compare">
        <div class="ui-preview-compare-col">
          <h4>Base: {branch}</h4>
          {[...allPaths].sort().map(path => {
            const entry = baseFiles.get(path);
            return (
              <div key={path} style="margin-bottom:0.75rem">
                <div class="text-xs text-muted mb-sm">{path.split('/').pop()}</div>
                {entry ? (
                  <img src={imageUrl(entry.artifact.pipeline_id, entry.file.id)}
                    style="width:100%;border-radius:var(--radius);border:1px solid var(--border)" loading="lazy" alt={path} />
                ) : (
                  <div class="empty-state" style="padding:2rem;font-size:0.75rem">Not in base</div>
                )}
              </div>
            );
          })}
        </div>
        <div class="ui-preview-compare-col">
          <h4>Head: {compareBranch}</h4>
          {[...allPaths].sort().map(path => {
            const entry = headFiles.get(path);
            return (
              <div key={path} style="margin-bottom:0.75rem">
                <div class="text-xs text-muted mb-sm">{path.split('/').pop()}</div>
                {entry ? (
                  <img src={imageUrl(entry.artifact.pipeline_id, entry.file.id)}
                    style="width:100%;border-radius:var(--radius);border:1px solid var(--border)" loading="lazy" alt={path} />
                ) : (
                  <div class="empty-state" style="padding:2rem;font-size:0.75rem">Not in head</div>
                )}
              </div>
            );
          })}
        </div>
      </div>
    );
  };

  if (loading) return <div class="empty-state">Loading...</div>;

  return (
    <div>
      <div class="flex gap-sm mb-md" style="align-items:center;flex-wrap:wrap">
        <select class="input" style="width:auto" value={branch}
          onChange={(e) => setBranch((e.target as HTMLSelectElement).value)}>
          {branches.map(b => <option key={b.name} value={b.name}>{b.name}</option>)}
        </select>
        <div class="flex gap-sm">
          {(['all', 'ui-comp', 'ui-flow'] as const).map(t => (
            <button key={t} class={`btn btn-sm${typeFilter === t ? ' btn-primary' : ''}`}
              onClick={() => setTypeFilter(t)}>
              {t === 'all' ? 'All' : t === 'ui-comp' ? 'Components' : 'Flows'}
            </button>
          ))}
        </div>
        <label style="margin-left:auto;display:flex;align-items:center;gap:0.4rem;font-size:0.8rem;color:var(--text-secondary);cursor:pointer">
          <input type="checkbox" checked={compareEnabled}
            onChange={() => { setCompareEnabled(!compareEnabled); if (compareEnabled) { setCompareBranch(''); setCompareData(null); } }} />
          Compare
        </label>
        {compareEnabled && (
          <select class="input" style="width:auto" value={compareBranch}
            onChange={(e) => setCompareBranch((e.target as HTMLSelectElement).value)}>
            <option value="">Select branch...</option>
            {branches.filter(b => b.name !== branch).map(b => (
              <option key={b.name} value={b.name}>{b.name}</option>
            ))}
          </select>
        )}
      </div>
      {allMeta.size > 0 && (
        <div class="flex gap-sm mb-md" style="flex-wrap:wrap;align-items:center">
          <span class="text-xs text-muted">Filter:</span>
          {[...allMeta.entries()].map(([key, values]) =>
            [...values].sort().map(v => (
              <span key={`${key}-${v}`}
                class={`ui-preview-meta-badge${metaFilter && metaFilter.key === key && metaFilter.value === v ? ' active' : ''}`}
                onClick={() => toggleMetaFilter(key, v)}>{key}: {v}</span>
            ))
          )}
          {metaFilter && <button class="btn btn-sm" onClick={() => setMetaFilter(null)}>Clear</button>}
        </div>
      )}
      {compareEnabled && compareBranch ? (
        <div class="card">{renderCompare()}</div>
      ) : (
        <div class="card">
          {artifacts.length === 0 ? (
            <div class="empty-state">
              <p>No UI previews yet.</p>
              <p class="text-muted text-sm mt-sm">
                Add a ui-preview step with artifacts to your .platform.yaml to get started.
              </p>
            </div>
          ) : (
            artifacts.map(artifact => (
              <div key={artifact.id} style="margin-bottom:1.5rem">
                <div class="flex-between mb-sm">
                  <h3 style="font-size:0.9rem">{artifact.name}</h3>
                  <Badge status={artifact.artifact_type === 'ui-comp' ? 'component' : 'flow'}>
                    {artifact.artifact_type === 'ui-comp' ? 'Component' : 'Flow'}
                  </Badge>
                </div>
                {artifact.config ? (
                  <>
                    {Object.entries(artifact.config.groups).map(([k, g]) => renderGroup(k, g, artifact, '', 0))}
                    {renderUncategorized(artifact)}
                  </>
                ) : renderUncategorized(artifact)}
              </div>
            ))
          )}
        </div>
      )}
      <Overlay open={!!lightbox} onClose={() => setLightbox(null)}
        title={lightbox?.item?.label || lightbox?.file.relative_path.split('/').pop() || 'Preview'}>
        {lightbox && (
          <div style="text-align:center">
            <img class="ui-preview-lightbox-img"
              src={imageUrl(lightbox.artifact.pipeline_id, lightbox.file.id)}
              alt={lightbox.item?.label || lightbox.file.relative_path}
              style="max-width:100%;max-height:70vh;border-radius:var(--radius)" />
            <div class="ui-preview-lightbox-meta">
              {lightbox.item?.meta && Object.entries(lightbox.item.meta).map(([k, v]) => (
                <span key={`${k}-${v}`} class="ui-preview-meta-badge">{k}: {v}</span>
              ))}
            </div>
            <div class="text-xs text-muted" style="text-align:center;margin-top:0.5rem">
              {lightbox.file.relative_path}
              {lightbox.file.size_bytes != null && ` (${Math.round(lightbox.file.size_bytes / 1024)} KB)`}
            </div>
          </div>
        )}
      </Overlay>
    </div>
  );
}

/* ---- Deployments Tab ---- */

interface DeployRelease {
  id: string;
  target_id: string;
  project_id: string;
  environment: string;
  image_ref: string;
  commit_sha: string | null;
  strategy: string;
  phase: string;
  traffic_weight: number;
  health: string;
  current_step: number;
  deployed_by: string | null;
  pipeline_id: string | null;
  started_at: string | null;
  completed_at: string | null;
  created_at: string;
  updated_at: string;
}

interface DeployTarget {
  id: string;
  project_id: string;
  name: string;
  environment: string;
  branch: string | null;
  branch_slug: string | null;
  ttl_hours: number | null;
  expires_at: string | null;
  default_strategy: string;
  is_active: boolean;
  created_at: string;
  updated_at: string;
}

export function DeploymentsTab({ projectId }: { projectId: string }) {
  const [targets, setTargets] = useState<DeployTarget[]>([]);
  const [releases, setReleases] = useState<DeployRelease[]>([]);
  const [selectedEnv, setSelectedEnv] = useState<string | null>(null);
  const [showRollback, setShowRollback] = useState(false);
  const [rollbackImage, setRollbackImage] = useState('');

  const load = () => {
    api.get<ListResponse<DeployTarget>>(`/api/projects/${projectId}/targets`)
      .then(r => setTargets(r.items)).catch(() => setTargets([]));
    api.get<ListResponse<DeployRelease>>(`/api/projects/${projectId}/deploy-releases?limit=50`)
      .then(r => setReleases(r.items)).catch(() => setReleases([]));
  };

  useEffect(() => {
    load();
    const interval = setInterval(load, 10000);
    return () => clearInterval(interval);
  }, [projectId]);

  // Build target_id → environment lookup for releases missing environment field
  const targetEnvMap = new Map<string, string>();
  for (const t of targets) targetEnvMap.set(t.id, t.environment);

  // Group releases by environment (resolve from target if missing)
  const envMap = new Map<string, DeployRelease[]>();
  for (const r of releases) {
    const env = r.environment || targetEnvMap.get(r.target_id) || '';
    if (!env) continue;
    const list = envMap.get(env) || [];
    list.push(r);
    envMap.set(env, list);
  }

  // Also include targets that have no releases yet
  for (const t of targets) {
    if (!envMap.has(t.environment)) envMap.set(t.environment, []);
  }

  const envNames = [...envMap.keys()].sort((a, b) => {
    const order: Record<string, number> = { production: 0, staging: 1, preview: 2 };
    return (order[a] ?? 99) - (order[b] ?? 99);
  });

  const rollback = async () => {
    if (!rollbackImage) return;
    try {
      await api.post(`/api/projects/${projectId}/deploy-releases`, {
        image_ref: rollbackImage,
        commit_sha: null,
      });
      setShowRollback(false); setRollbackImage(''); load();
    } catch { /* ignore */ }
  };

  const statusColor = (s: string): string => {
    if (s === 'healthy' || s === 'complete' || s === 'running') return 'var(--success)';
    if (s === 'progressing' || s === 'pending' || s === 'canary') return 'var(--warning)';
    if (s === 'failed' || s === 'rolled_back') return 'var(--danger)';
    return 'var(--text-muted)';
  };

  // Preview targets (with TTL)
  const previewTargets = targets.filter(t => t.environment === 'preview' && t.expires_at);
  const timeRemaining = (expiresAt: string): string => {
    const ms = new Date(expiresAt).getTime() - Date.now();
    if (ms <= 0) return 'Expired';
    const hours = Math.floor(ms / 3600000);
    if (hours > 0) return `${hours}h left`;
    return `${Math.floor(ms / 60000)}m left`;
  };

  return (
    <div>
      <div class="env-cards mb-md">
        {envNames.map(env => {
          const rels = envMap.get(env) || [];
          const latest = rels[0];
          return (
            <div key={env} class={`env-card ${selectedEnv === env ? 'env-card-selected' : ''}`}
              onClick={() => setSelectedEnv(selectedEnv === env ? null : env)}>
              <div class="env-card-name">{env}</div>
              {latest ? (
                <div>
                  <div class="flex gap-sm" style="align-items:center">
                    <span class="status-dot" style={`background:${statusColor(latest.health || latest.phase)}`} />
                    <span class="text-sm">{latest.health || latest.phase}</span>
                  </div>
                  <div class="mono text-xs mt-sm truncate">{latest.image_ref}</div>
                  <div class="text-muted text-xs mt-sm">{timeAgo(latest.created_at)}</div>
                </div>
              ) : (
                <div class="text-muted text-sm">No releases</div>
              )}
              {env !== 'preview' && latest && (
                <button class="btn btn-sm mt-sm" onClick={(e) => {
                  e.stopPropagation(); setSelectedEnv(env); setShowRollback(true);
                }}>Rollback</button>
              )}
            </div>
          );
        })}
        {envNames.length === 0 && <div class="empty-state" style="width:100%">No deployments</div>}
      </div>

      {selectedEnv && (
        <div class="card mb-md">
          <div class="card-header"><span class="card-title">Release History ({selectedEnv})</span></div>
          <table class="table">
            <thead><tr><th>Time</th><th>Image</th><th>Phase</th><th>Health</th><th>Strategy</th></tr></thead>
            <tbody>
              {(envMap.get(selectedEnv) || []).map(r => (
                <tr key={r.id}>
                  <td class="text-muted text-sm">{timeAgo(r.created_at)}</td>
                  <td class="mono text-xs truncate" style="max-width:200px">{r.image_ref}</td>
                  <td><Badge status={r.phase} /></td>
                  <td><Badge status={r.health} /></td>
                  <td class="text-sm">{r.strategy}</td>
                </tr>
              ))}
              {(envMap.get(selectedEnv) || []).length === 0 && (
                <tr><td colSpan={5} class="text-muted text-sm" style="text-align:center;padding:1rem">No releases yet</td></tr>
              )}
            </tbody>
          </table>
        </div>
      )}

      {previewTargets.length > 0 && (
        <div class="card">
          <div class="card-header"><span class="card-title">Active Previews</span></div>
          <table class="table">
            <thead><tr><th>Branch</th><th>Expires</th><th>Created</th></tr></thead>
            <tbody>
              {previewTargets.map(t => (
                <tr key={t.id}>
                  <td class="mono text-xs">{t.branch || t.name}</td>
                  <td class="text-sm">{t.expires_at ? timeRemaining(t.expires_at) : '--'}</td>
                  <td class="text-muted text-sm">{timeAgo(t.created_at)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}

      <Modal open={showRollback} onClose={() => setShowRollback(false)} title={`Rollback ${selectedEnv}`}>
        <div class="form-group">
          <label>Image to deploy</label>
          <input class="input" value={rollbackImage} placeholder="Enter image reference..."
            onInput={(e) => setRollbackImage((e.target as HTMLInputElement).value)} />
        </div>
        <div class="text-sm text-muted mb-md">This will create a new release with the specified image.</div>
        <div class="modal-actions">
          <button class="btn" onClick={() => setShowRollback(false)}>Cancel</button>
          <button class="btn btn-primary" onClick={rollback} disabled={!rollbackImage}>Deploy</button>
        </div>
      </Modal>
    </div>
  );
}

/* ---- Sessions Tab (re-export) ---- */

export { Sessions as SessionsTab } from '../pages/Sessions';

/* ---- Logs Tab ---- */

export function LogsTab({ projectId }: { projectId: string }) {
  const [logs, setLogs] = useState<LogEntry[]>([]);
  const [total, setTotal] = useState(0);
  const [offset, setOffset] = useState(0);
  const [filters, setFilters] = useState<Record<string, string>>({ range: '24h', level: '', source: '' });
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  const [loading, setLoading] = useState(false);

  const load = () => {
    setLoading(true);
    const params: Record<string, string | number> = { limit: 50, offset };
    if (filters.range) params.range = filters.range;
    if (filters.level) params.level = filters.level;
    if (filters.source) params.source = filters.source;
    if (filters.q) params.q = filters.q;
    api.get<ListResponse<LogEntry>>(`/api/projects/${projectId}/logs${qs(params)}`)
      .then(r => { setLogs(r.items); setTotal(r.total); })
      .catch(e => console.warn(e))
      .finally(() => setLoading(false));
  };
  useEffect(load, [offset, projectId]);

  const toggleExpand = (id: string) => {
    setExpanded(prev => { const next = new Set(prev); if (next.has(id)) next.delete(id); else next.add(id); return next; });
  };
  const formatTime = (ts: string) => {
    const d = new Date(ts);
    return d.toLocaleTimeString('en-US', { hour12: false, hour: '2-digit', minute: '2-digit', second: '2-digit' });
  };
  const LEVEL_CLASSES: Record<string, string> = {
    error: 'log-level-error', warn: 'log-level-warn', info: 'log-level-info',
    debug: 'log-level-debug', trace: 'log-level-trace',
  };

  return (
    <div>
      <FilterBar filters={[
        { key: 'range', label: 'Time range', type: 'select', options: [
          { value: '1h', label: 'Last 1 hour' }, { value: '6h', label: 'Last 6 hours' },
          { value: '24h', label: 'Last 24 hours' }, { value: '7d', label: 'Last 7 days' },
        ] },
        { key: 'level', label: 'Level', type: 'select', options: [
          { value: '', label: 'All levels' }, { value: 'error', label: 'Error' },
          { value: 'warn', label: 'Warn' }, { value: 'info', label: 'Info' },
        ] },
        { key: 'source', label: 'Source', type: 'select', options: [
          { value: '', label: 'All sources' }, { value: 'system', label: 'System' },
          { value: 'api', label: 'API' }, { value: 'session', label: 'Session' },
          { value: 'external', label: 'External' },
        ] },
        { key: 'q', label: 'Search', type: 'text', placeholder: 'Full-text search...' },
      ]} values={filters} onChange={setFilters} onApply={() => { setOffset(0); load(); }} />
      <div class="card" style="margin-top:1rem">
        {loading ? <div class="empty-state">Loading...</div> : logs.length === 0 ? (
          <div class="empty-state">No log entries found</div>
        ) : (
          <div class="log-list">
            {logs.map(entry => (
              <div key={entry.id} class="log-entry" onClick={() => toggleExpand(entry.id)}>
                <div class="log-entry-row">
                  <span class="log-time mono text-xs">{formatTime(entry.timestamp)}</span>
                  <span class={`log-level ${LEVEL_CLASSES[entry.level.toLowerCase()] || ''}`}>
                    {entry.level.toUpperCase().padEnd(5)}
                  </span>
                  <span class="log-source text-xs" style="opacity:0.6">{entry.source}</span>
                  <span class="log-service text-xs">{entry.service}</span>
                  <span class="log-message">{entry.message}</span>
                </div>
                {expanded.has(entry.id) && entry.attributes && (
                  <div class="log-attributes">
                    <pre class="log-viewer" style="max-height:200px;margin-top:0.5rem">
                      {JSON.stringify(entry.attributes, null, 2)}
                    </pre>
                  </div>
                )}
              </div>
            ))}
          </div>
        )}
        <Pagination total={total} limit={50} offset={offset} onChange={setOffset} />
      </div>
    </div>
  );
}

/* ---- Skills Tab ---- */

interface ResolvedCommand {
  name: string;
  prompt_template: string;
  scope: string;
  persistent_session: boolean;
}

export function SkillsTab({ projectId }: { projectId: string }) {
  const [resolved, setResolved] = useState<ResolvedCommand[]>([]);
  const [showCreate, setShowCreate] = useState(false);
  const [form, setForm] = useState({ name: '', description: '', prompt_template: '', persistent_session: false });
  const [error, setError] = useState('');

  const load = () => {
    api.get<ResolvedCommand[]>(`/api/commands/resolved${qs({ project_id: projectId })}`)
      .then(setResolved).catch(e => console.warn(e));
  };
  useEffect(load, [projectId]);

  const create = async (e: Event) => {
    e.preventDefault(); setError('');
    try {
      await api.post('/api/commands', { ...form, project_id: projectId });
      setShowCreate(false); setForm({ name: '', description: '', prompt_template: '', persistent_session: false }); load();
    } catch (err: any) { setError(err.message); }
  };

  return (
    <div>
      <div class="flex-between mb-md">
        <span class="text-muted text-sm">
          Showing resolved skills (project overrides workspace overrides global). Repo commands (.claude/commands/) take highest priority at runtime.
        </span>
        <button class="btn btn-primary btn-sm" onClick={() => { setShowCreate(true); setError(''); }}>New Project Skill</button>
      </div>
      <div class="card">
        {resolved.length === 0 ? <div class="empty-state">No skills defined</div> : (
          <table class="table">
            <thead><tr><th>Name</th><th>Scope</th><th>Persistent</th></tr></thead>
            <tbody>
              {resolved.map(c => (
                <tr key={c.name}>
                  <td class="mono">/{c.name}</td>
                  <td><Badge status={c.scope} /></td>
                  <td>{c.persistent_session ? 'Yes' : ''}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>
      <Modal open={showCreate} onClose={() => setShowCreate(false)} title="New Project Skill">
        <form onSubmit={create}>
          <div class="form-group">
            <label>Name</label>
            <input class="input" required placeholder="e.g. dev, review" value={form.name}
              onInput={(e) => setForm({ ...form, name: (e.target as HTMLInputElement).value })} />
          </div>
          <div class="form-group">
            <label>Description</label>
            <input class="input" value={form.description}
              onInput={(e) => setForm({ ...form, description: (e.target as HTMLInputElement).value })} />
          </div>
          <div class="form-group">
            <label>Prompt Template</label>
            <textarea class="input mono" rows={8} required value={form.prompt_template}
              placeholder="Use $ARGUMENTS for user input"
              onInput={(e) => setForm({ ...form, prompt_template: (e.target as HTMLTextAreaElement).value })} />
          </div>
          <div class="form-group">
            <label>
              <input type="checkbox" checked={form.persistent_session}
                onChange={() => setForm({ ...form, persistent_session: !form.persistent_session })} />
              {' '}Persistent session
            </label>
          </div>
          {error && <div class="error-msg">{error}</div>}
          <div class="modal-actions">
            <button type="button" class="btn" onClick={() => setShowCreate(false)}>Cancel</button>
            <button type="submit" class="btn btn-primary">Create</button>
          </div>
        </form>
      </Modal>
    </div>
  );
}

/* ---- Webhooks Tab ---- */

export function WebhooksTab({ projectId }: { projectId: string }) {
  const [webhooks, setWebhooks] = useState<Webhook[]>([]);
  const [showCreate, setShowCreate] = useState(false);
  const [form, setForm] = useState({ url: '', events: ['push'], secret: '' });
  const [error, setError] = useState('');

  const load = () => {
    api.get<ListResponse<Webhook>>(`/api/projects/${projectId}/webhooks?limit=50`)
      .then(r => setWebhooks(r.items)).catch(e => console.warn(e));
  };
  useEffect(load, [projectId]);

  const create = async (e: Event) => {
    e.preventDefault();
    try {
      await api.post(`/api/projects/${projectId}/webhooks`, {
        url: form.url, events: form.events, secret: form.secret || undefined,
      });
      setShowCreate(false); setForm({ url: '', events: ['push'], secret: '' }); load();
    } catch (err: any) { setError(err.message); }
  };
  const remove = async (whId: string) => {
    await api.del(`/api/projects/${projectId}/webhooks/${whId}`);
    load();
  };

  return (
    <div>
      <div class="flex-between mb-md">
        <span />
        <button class="btn btn-primary btn-sm" onClick={() => setShowCreate(true)}>New Webhook</button>
      </div>
      <div class="card">
        {webhooks.length === 0 ? <div class="empty-state">No webhooks</div> : (
          <table class="table">
            <thead><tr><th>URL</th><th>Events</th><th>Active</th><th></th></tr></thead>
            <tbody>
              {webhooks.map(w => (
                <tr key={w.id}>
                  <td class="mono text-xs truncate" style="max-width:250px">{w.url}</td>
                  <td class="text-xs">{w.events.join(', ')}</td>
                  <td><Badge status={w.active ? 'active' : 'inactive'} /></td>
                  <td><button class="btn btn-danger btn-sm" onClick={() => remove(w.id)}>Delete</button></td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>
      <Modal open={showCreate} onClose={() => setShowCreate(false)} title="New Webhook">
        <form onSubmit={create}>
          <div class="form-group">
            <label>URL</label>
            <input class="input" type="url" required value={form.url}
              onInput={(e) => setForm({ ...form, url: (e.target as HTMLInputElement).value })} />
          </div>
          <div class="form-group">
            <label>Events (comma-separated)</label>
            <input class="input" value={form.events.join(',')}
              onInput={(e) => setForm({ ...form, events: (e.target as HTMLInputElement).value.split(',').map(s => s.trim()).filter(Boolean) })} />
          </div>
          <div class="form-group">
            <label>Secret (optional)</label>
            <input class="input" type="password" value={form.secret}
              onInput={(e) => setForm({ ...form, secret: (e.target as HTMLInputElement).value })} />
          </div>
          {error && <div class="error-msg">{error}</div>}
          <div class="modal-actions">
            <button type="button" class="btn" onClick={() => setShowCreate(false)}>Cancel</button>
            <button type="submit" class="btn btn-primary">Create</button>
          </div>
        </form>
      </Modal>
    </div>
  );
}

/* ---- Settings Tab ---- */

export function SettingsTab({ project, onUpdate }: { project: Project; onUpdate: (p: Project) => void }) {
  const [form, setForm] = useState({
    display_name: project.display_name || '',
    description: project.description || '',
    visibility: project.visibility,
    default_branch: project.default_branch,
  });
  const [saved, setSaved] = useState(false);
  const [error, setError] = useState('');

  const save = async (e: Event) => {
    e.preventDefault(); setError('');
    try {
      const updated = await api.patch<Project>(`/api/projects/${project.id}`, form);
      onUpdate(updated); setSaved(true); setTimeout(() => setSaved(false), 2000);
    } catch (err: any) { setError(err.message); }
  };

  return (
    <div>
      <div class="card mb-md">
        <div class="card-title mb-md">Project Settings</div>
        <form onSubmit={save}>
          <div class="form-group">
            <label>Display Name</label>
            <input class="input" value={form.display_name}
              onInput={(e) => setForm({ ...form, display_name: (e.target as HTMLInputElement).value })} />
          </div>
          <div class="form-group">
            <label>Description</label>
            <textarea class="input" value={form.description}
              onInput={(e) => setForm({ ...form, description: (e.target as HTMLTextAreaElement).value })} />
          </div>
          <div class="form-group">
            <label>Visibility</label>
            <select class="input" value={form.visibility}
              onChange={(e) => setForm({ ...form, visibility: (e.target as HTMLSelectElement).value })}>
              <option value="private">Private</option>
              <option value="internal">Internal</option>
              <option value="public">Public</option>
            </select>
          </div>
          <div class="form-group">
            <label>Default Branch</label>
            <input class="input" value={form.default_branch}
              onInput={(e) => setForm({ ...form, default_branch: (e.target as HTMLInputElement).value })} />
          </div>
          {error && <div class="error-msg">{error}</div>}
          {saved && <div style="color:var(--success);font-size:13px">Saved</div>}
          <button type="submit" class="btn btn-primary mt-sm">Save Settings</button>
        </form>
      </div>
      {(project.namespace_slug || project.agent_image) && (
        <div class="card mb-md">
          <div class="card-title mb-md">Agent Settings</div>
          <div class="session-meta-list">
            <div class="session-meta-row">
              <span class="text-muted text-sm">Namespace</span>
              <span class="mono text-sm">{project.namespace_slug}</span>
            </div>
            {project.agent_image && (
              <div class="session-meta-row">
                <span class="text-muted text-sm">Agent Image</span>
                <span class="mono text-sm" style="word-break:break-all">{project.agent_image}</span>
              </div>
            )}
          </div>
        </div>
      )}
    </div>
  );
}

/* ---- Secrets Tab (standalone) ---- */

export function SecretsTab({ projectId }: { projectId: string }) {
  const [secrets, setSecrets] = useState<Secret[]>([]);
  const [showCreate, setShowCreate] = useState(false);
  const [form, setForm] = useState({ name: '', value: '', scope: 'build' });
  const [error, setError] = useState('');

  const load = () => {
    api.get<ListResponse<Secret>>(`/api/projects/${projectId}/secrets?limit=100`)
      .then(r => setSecrets(r.items)).catch(() => setSecrets([]));
  };
  useEffect(load, [projectId]);

  const create = async (e: Event) => {
    e.preventDefault(); setError('');
    try {
      await api.post(`/api/projects/${projectId}/secrets`, { name: form.name, value: form.value, scope: form.scope });
      setShowCreate(false); setForm({ name: '', value: '', scope: 'build' }); load();
    } catch (err: any) { setError(err.message); }
  };
  const deleteSecret = async (name: string) => {
    if (!confirm(`Delete secret "${name}"? This action cannot be undone.`)) return;
    await api.del(`/api/projects/${projectId}/secrets/${encodeURIComponent(name)}`);
    load();
  };

  return (
    <div>
      <div class="flex-between mb-md">
        <span class="text-muted text-sm">Secret values are encrypted and cannot be displayed after creation.</span>
        <button class="btn btn-primary btn-sm" onClick={() => setShowCreate(true)}>Add Secret</button>
      </div>
      <div class="card">
        {secrets.length === 0 ? <div class="empty-state">No secrets configured</div> : (
          <table class="table">
            <thead><tr><th>Name</th><th>Scope</th><th>Version</th><th>Updated</th><th></th></tr></thead>
            <tbody>
              {secrets.map(s => (
                <tr key={s.id}>
                  <td class="mono text-sm">{s.name}</td>
                  <td class="text-sm"><Badge status={s.scope} /></td>
                  <td class="text-sm text-muted">v{s.version}</td>
                  <td class="text-muted text-sm">{timeAgo(s.updated_at)}</td>
                  <td><button class="btn btn-danger btn-sm" onClick={() => deleteSecret(s.name)}>Delete</button></td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>
      <Modal open={showCreate} onClose={() => setShowCreate(false)} title="Add Secret">
        <form onSubmit={create}>
          <div class="form-group">
            <label>Name</label>
            <input class="input" required value={form.name} placeholder="SECRET_NAME"
              onInput={(e) => setForm({ ...form, name: (e.target as HTMLInputElement).value })} />
          </div>
          <div class="form-group">
            <label>Value</label>
            <textarea class="input" required value={form.value} rows={3}
              onInput={(e) => setForm({ ...form, value: (e.target as HTMLTextAreaElement).value })} />
            <div class="text-xs mt-sm" style="color:var(--warning)">This value will not be shown again after creation.</div>
          </div>
          <div class="form-group">
            <label>Scope</label>
            <select class="input" value={form.scope}
              onChange={(e) => setForm({ ...form, scope: (e.target as HTMLSelectElement).value })}>
              <option value="build">Build</option>
              <option value="deploy">Deploy</option>
              <option value="all">All</option>
            </select>
          </div>
          {error && <div class="error-msg">{error}</div>}
          <div class="modal-actions">
            <button type="button" class="btn" onClick={() => setShowCreate(false)}>Cancel</button>
            <button type="submit" class="btn btn-primary">Create</button>
          </div>
        </form>
      </Modal>
    </div>
  );
}

/* ---- Docs Tab ---- */

// Hierarchical docs.json schema (up to 3 levels)
// Leaf: { id, file, title } — file is .md or .mmd
// Group: { title, children: [...] }
interface DocLeaf { id: string; file: string; title: string; }
interface DocGroup { title: string; children: DocNode[]; }
type DocNode = DocLeaf | DocGroup;
function isDocLeaf(n: DocNode): n is DocLeaf { return 'file' in n; }

// Legacy flat schema
interface LegacyDocEntry { id: string; file: string; title: string; section: number; }
interface LegacyManifest { docs: LegacyDocEntry[]; }

// Convert legacy flat → tree
function legacyToTree(docs: LegacyDocEntry[]): DocNode[] {
  return docs.sort((a, b) => a.section - b.section).map(d => ({ id: d.id, file: d.file, title: d.title }));
}

// Find the first leaf in a tree
function firstLeaf(nodes: DocNode[]): DocLeaf | null {
  for (const n of nodes) {
    if (isDocLeaf(n)) return n;
    const found = firstLeaf((n as DocGroup).children);
    if (found) return found;
  }
  return null;
}

// Sidebar tree renderer
function DocsSidebarTree({ nodes, activeFile, depth, onSelect }: {
  nodes: DocNode[]; activeFile: string | null; depth: number;
  onSelect: (file: string) => void;
}) {
  const [collapsed, setCollapsed] = useState<Set<string>>(new Set());
  const toggle = (title: string) => {
    setCollapsed(prev => { const n = new Set(prev); if (n.has(title)) n.delete(title); else n.add(title); return n; });
  };

  return (
    <>
      {nodes.map((node, i) => {
        if (isDocLeaf(node)) {
          const isDiagram = node.file.endsWith('.mmd');
          return (
            <button key={node.id}
              class={`docs-sidebar-item${activeFile === node.file ? ' active' : ''}`}
              style={`padding-left: ${0.5 + depth * 0.75}rem`}
              onClick={() => onSelect(node.file)}>
              {isDiagram && <span class="docs-icon-diagram" title="Diagram">&#9670; </span>}
              {node.title}
            </button>
          );
        }
        const group = node as DocGroup;
        const key = `${depth}-${i}-${group.title}`;
        const isCollapsed = collapsed.has(key);
        return (
          <div key={key}>
            <button class="docs-sidebar-group" style={`padding-left: ${0.5 + depth * 0.75}rem`}
              onClick={() => toggle(key)}>
              <span class="docs-sidebar-chevron">{isCollapsed ? '\u25B8' : '\u25BE'}</span>
              {group.title}
            </button>
            {!isCollapsed && (
              <DocsSidebarTree nodes={group.children} activeFile={activeFile}
                depth={depth + 1} onSelect={onSelect} />
            )}
          </div>
        );
      })}
    </>
  );
}

export function DocsTab({ projectId, defaultBranch }: { projectId: string; defaultBranch: string }) {
  const [tree, setTree] = useState<DocNode[]>([]);
  const [activeFile, setActiveFile] = useState<string | null>(null);
  const [content, setContent] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [gitRef, setRef] = useState(defaultBranch);
  const [branches, setBranches] = useState<BranchInfo[]>([]);
  const [docsPrefix, setDocsPrefix] = useState('docs/');
  // Diagram zoom overlay
  const [zoomSvg, setZoomSvg] = useState<string | null>(null);
  const [zoomTitle, setZoomTitle] = useState('');

  useEffect(() => {
    api.get<BranchInfo[]>(`/api/projects/${projectId}/branches`).then(setBranches).catch(() => {});
  }, [projectId]);

  // Load manifest
  useEffect(() => {
    setLoading(true);
    setTree([]);
    setActiveFile(null);
    setContent(null);

    const tryManifest = (path: string, prefix: string): Promise<boolean> =>
      api.get<{ content: string; encoding: string }>(
        `/api/projects/${projectId}/blob${qs({ ref: gitRef, path })}`
      ).then(blob => {
        const text = blob.encoding === 'base64' ? atob(blob.content) : blob.content;
        const parsed = JSON.parse(text);
        setDocsPrefix(prefix);
        let nodes: DocNode[];
        if (parsed.tree) {
          nodes = parsed.tree;
        } else if (parsed.docs) {
          nodes = legacyToTree(parsed.docs as LegacyDocEntry[]);
        } else {
          return false;
        }
        setTree(nodes);
        const first = firstLeaf(nodes);
        if (first) setActiveFile(first.file);
        setLoading(false);
        return true;
      }).catch(() => false);

    // Try docs/docs.json → docs/arc42/docs.json → fallback dir listing
    tryManifest('docs/docs.json', 'docs/').then(ok => {
      if (ok) return;
      return tryManifest('docs/arc42/docs.json', 'docs/arc42/').then(ok2 => {
        if (ok2) return;
        // Fallback: list docs/ for .md files
        api.get<TreeEntry[]>(`/api/projects/${projectId}/tree${qs({ ref: gitRef, path: 'docs' })}`)
          .then(entries => {
            const mds = entries
              .filter(e => e.entry_type === 'blob' && (e.name.endsWith('.md') || e.name.endsWith('.mmd')))
              .sort((a, b) => a.name.localeCompare(b.name));
            const nodes: DocNode[] = mds.map(f => ({
              id: f.name, file: f.name,
              title: f.name.replace(/^\d+-/, '').replace(/\.(md|mmd)$/, '').replace(/-/g, ' '),
            }));
            setDocsPrefix('docs/');
            setTree(nodes);
            if (nodes.length > 0 && isDocLeaf(nodes[0])) setActiveFile((nodes[0] as DocLeaf).file);
            setLoading(false);
          })
          .catch(() => setLoading(false));
      });
    });
  }, [projectId, gitRef]);

  // Load active file content
  useEffect(() => {
    if (!activeFile) { setContent(null); return; }
    setContent(null);
    const fullPath = `${docsPrefix}${activeFile}`;
    api.get<{ content: string; encoding: string }>(
      `/api/projects/${projectId}/blob${qs({ ref: gitRef, path: fullPath })}`
    )
      .then(blob => setContent(blob.encoding === 'base64' ? atob(blob.content) : blob.content))
      .catch(() => setContent('*Failed to load document.*'));
  }, [activeFile, projectId, gitRef, docsPrefix]);

  const isDiagram = activeFile?.endsWith('.mmd');

  const openDiagramZoom = (mermaidCode: string, title: string) => {
    renderMermaid(mermaidCode)
      .then(result => { setZoomSvg(result.svg); setZoomTitle(title); })
      .catch(() => {});
  };

  if (loading) return <div class="empty-state">Loading documentation...</div>;

  if (tree.length === 0) {
    return (
      <div class="card">
        <div class="empty-state">
          <p>No documentation found</p>
          <p class="text-muted text-sm mt-sm">
            Add a <code>docs/</code> directory with <code>.md</code> files to your repo.
            Optionally include a <code>docs/docs.json</code> manifest for ordered navigation.
          </p>
        </div>
      </div>
    );
  }

  return (
    <div class="docs-viewer">
      <div class="docs-sidebar">
        <div class="mb-sm">
          <select class="input" style="width:100%;font-size:0.75rem;padding:0.25rem 0.5rem" value={gitRef}
            onChange={(e) => setRef((e.target as HTMLSelectElement).value)}>
            {branches.map(b => <option key={b.name} value={b.name}>{b.name}</option>)}
          </select>
        </div>
        <DocsSidebarTree nodes={tree} activeFile={activeFile} depth={0} onSelect={setActiveFile} />
      </div>
      <div class="docs-content">
        {content === null ? (
          <div class="text-muted text-sm" style="padding:2rem;text-align:center">Loading...</div>
        ) : isDiagram ? (
          <div class="mermaid-block">
            <MermaidBlock code={content}
              onClick={() => openDiagramZoom(content, activeFile?.replace(/\.(mmd)$/, '') || 'Diagram')} />
            <div class="text-xs text-muted mt-sm" style="text-align:center">Click diagram to zoom</div>
          </div>
        ) : (
          <Markdown content={content} />
        )}
      </div>

      {/* Zoomable diagram overlay */}
      <Overlay open={!!zoomSvg} onClose={() => setZoomSvg(null)} title={zoomTitle}>
        {zoomSvg && <ZoomableDiagram svg={zoomSvg} />}
      </Overlay>
    </div>
  );
}

/* ---- Observe Tab (re-exported from dedicated file) ---- */

export { ObserveTab } from './ObserveTab';
