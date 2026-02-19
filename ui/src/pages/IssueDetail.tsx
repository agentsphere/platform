import { useState, useEffect } from 'preact/hooks';
import { api, type ListResponse } from '../lib/api';
import type { Issue, Comment } from '../lib/types';
import { timeAgo } from '../lib/format';
import { Badge } from '../components/Badge';
import { Markdown } from '../components/Markdown';

interface Props { id?: string; number?: string; }

export function IssueDetail({ id: projectId, number: num }: Props) {
  const [issue, setIssue] = useState<Issue | null>(null);
  const [comments, setComments] = useState<Comment[]>([]);
  const [body, setBody] = useState('');
  const [error, setError] = useState('');

  useEffect(() => {
    if (!projectId || !num) return;
    api.get<Issue>(`/api/projects/${projectId}/issues/${num}`).then(setIssue).catch(() => {});
    loadComments();
  }, [projectId, num]);

  const loadComments = () => {
    api.get<ListResponse<Comment>>(`/api/projects/${projectId}/issues/${num}/comments?limit=100`)
      .then(r => setComments(r.items)).catch(() => {});
  };

  const addComment = async (e: Event) => {
    e.preventDefault();
    if (!body.trim()) return;
    try {
      await api.post(`/api/projects/${projectId}/issues/${num}/comments`, { body });
      setBody('');
      loadComments();
    } catch (err: any) { setError(err.message); }
  };

  const toggleStatus = async () => {
    if (!issue) return;
    const newStatus = issue.status === 'open' ? 'closed' : 'open';
    const updated = await api.patch<Issue>(`/api/projects/${projectId}/issues/${num}`, { status: newStatus });
    setIssue(updated);
  };

  if (!issue) return <div class="empty-state">Loading...</div>;

  return (
    <div>
      <div class="mb-md">
        <a href={`/projects/${projectId}/issues`} class="text-sm text-muted">Back to issues</a>
      </div>
      <div class="flex-between mb-md">
        <h2>{issue.title} <span class="text-muted">#{issue.number}</span></h2>
        <div class="flex gap-sm">
          <Badge status={issue.status} />
          <button class="btn btn-sm" onClick={toggleStatus}>
            {issue.status === 'open' ? 'Close' : 'Reopen'}
          </button>
        </div>
      </div>

      {issue.labels.length > 0 && (
        <div class="labels mb-md">
          {issue.labels.map(l => <span key={l} class="label-tag">{l}</span>)}
        </div>
      )}

      {issue.body && (
        <div class="card">
          <Markdown content={issue.body} />
        </div>
      )}

      <h3 class="mt-md mb-md" style="font-size:1rem">Comments ({comments.length})</h3>
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
            <textarea class="input" value={body} rows={3}
              onInput={(e) => setBody((e.target as HTMLTextAreaElement).value)} />
          </div>
          {error && <div class="error-msg">{error}</div>}
          <button type="submit" class="btn btn-primary btn-sm">Comment</button>
        </form>
      </div>
    </div>
  );
}
