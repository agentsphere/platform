import { useState, useEffect } from 'preact/hooks';
import { api, type ListResponse } from '../lib/api';
import type { MergeRequest, Review, Comment } from '../lib/types';
import { timeAgo } from '../lib/format';
import { Badge } from '../components/Badge';
import { Markdown } from '../components/Markdown';

interface Props { id?: string; number?: string; }

export function MRDetail({ id: projectId, number: num }: Props) {
  const [mr, setMr] = useState<MergeRequest | null>(null);
  const [reviews, setReviews] = useState<Review[]>([]);
  const [comments, setComments] = useState<Comment[]>([]);
  const [commentBody, setCommentBody] = useState('');
  const [reviewForm, setReviewForm] = useState({ verdict: 'approve', body: '' });
  const [error, setError] = useState('');

  useEffect(() => {
    if (!projectId || !num) return;
    api.get<MergeRequest>(`/api/projects/${projectId}/merge-requests/${num}`).then(setMr).catch(() => {});
    loadReviews();
    loadComments();
  }, [projectId, num]);

  const loadReviews = () => {
    api.get<ListResponse<Review>>(`/api/projects/${projectId}/merge-requests/${num}/reviews?limit=100`)
      .then(r => setReviews(r.items)).catch(() => {});
  };

  const loadComments = () => {
    api.get<ListResponse<Comment>>(`/api/projects/${projectId}/merge-requests/${num}/comments?limit=100`)
      .then(r => setComments(r.items)).catch(() => {});
  };

  const addComment = async (e: Event) => {
    e.preventDefault();
    if (!commentBody.trim()) return;
    try {
      await api.post(`/api/projects/${projectId}/merge-requests/${num}/comments`, { body: commentBody });
      setCommentBody('');
      loadComments();
    } catch (err: any) { setError(err.message); }
  };

  const submitReview = async (e: Event) => {
    e.preventDefault();
    try {
      await api.post(`/api/projects/${projectId}/merge-requests/${num}/reviews`, reviewForm);
      setReviewForm({ verdict: 'approve', body: '' });
      loadReviews();
    } catch (err: any) { setError(err.message); }
  };

  const merge = async () => {
    try {
      const updated = await api.post<MergeRequest>(`/api/projects/${projectId}/merge-requests/${num}/merge`);
      setMr(updated);
    } catch (err: any) { setError(err.message); }
  };

  if (!mr) return <div class="empty-state">Loading...</div>;

  return (
    <div>
      <div class="mb-md">
        <a href={`/projects/${projectId}/mrs`} class="text-sm text-muted">Back to merge requests</a>
      </div>
      <div class="flex-between mb-md">
        <h2>{mr.title} <span class="text-muted">#{mr.number}</span></h2>
        <Badge status={mr.status} />
      </div>

      <div class="flex gap-sm mb-md text-sm">
        <span class="mono">{mr.source_branch}</span>
        <span class="text-muted">→</span>
        <span class="mono">{mr.target_branch}</span>
        <span class="text-muted">· {timeAgo(mr.created_at)}</span>
      </div>

      {mr.body && (
        <div class="card">
          <Markdown content={mr.body} />
        </div>
      )}

      {mr.status === 'open' && (
        <div class="flex gap-sm mt-md mb-md">
          <button class="btn btn-primary" onClick={merge}>Merge</button>
        </div>
      )}

      <h3 class="mt-md mb-md" style="font-size:1rem">Reviews ({reviews.length})</h3>
      {reviews.map(r => (
        <div key={r.id} class="comment-box">
          <div class="comment-header">
            <Badge status={r.verdict} />
            <span style="margin-left:0.5rem">{timeAgo(r.created_at)}</span>
          </div>
          {r.body && <Markdown content={r.body} />}
        </div>
      ))}

      {mr.status === 'open' && (
        <div class="card mt-md">
          <form onSubmit={submitReview}>
            <div class="form-group">
              <label>Add Review</label>
              <select class="input" value={reviewForm.verdict}
                onChange={(e) => setReviewForm({ ...reviewForm, verdict: (e.target as HTMLSelectElement).value })}>
                <option value="approve">Approve</option>
                <option value="request_changes">Request Changes</option>
                <option value="comment">Comment</option>
              </select>
            </div>
            <div class="form-group">
              <label>Body (optional)</label>
              <textarea class="input" value={reviewForm.body}
                onInput={(e) => setReviewForm({ ...reviewForm, body: (e.target as HTMLTextAreaElement).value })} />
            </div>
            <button type="submit" class="btn btn-primary btn-sm">Submit Review</button>
          </form>
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
            <textarea class="input" value={commentBody} rows={3}
              onInput={(e) => setCommentBody((e.target as HTMLTextAreaElement).value)} />
          </div>
          {error && <div class="error-msg">{error}</div>}
          <button type="submit" class="btn btn-primary btn-sm">Comment</button>
        </form>
      </div>
    </div>
  );
}
