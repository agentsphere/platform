-- comments: listing comments for an issue
-- Query: SELECT ... FROM comments WHERE issue_id = $1 ORDER BY created_at ASC
CREATE INDEX idx_comments_issue ON comments(issue_id, created_at ASC)
  WHERE issue_id IS NOT NULL;

-- comments: listing comments for an MR
-- Query: SELECT ... FROM comments WHERE mr_id = $1 ORDER BY created_at ASC
CREATE INDEX idx_comments_mr ON comments(mr_id, created_at ASC)
  WHERE mr_id IS NOT NULL;

-- webhooks: listing + fire_webhooks dispatch
-- Query: SELECT ... FROM webhooks WHERE project_id = $1 AND active = true
CREATE INDEX idx_webhooks_project ON webhooks(project_id)
  WHERE active = true;

-- agent_messages: listing messages + progress fetch + idle check
-- Query: SELECT ... FROM agent_messages WHERE session_id = $1 ORDER BY created_at
CREATE INDEX idx_agent_messages_session ON agent_messages(session_id, created_at);

-- mr_reviews: listing reviews + approval count for merge validation
-- Query: SELECT ... FROM mr_reviews WHERE mr_id = $1 ORDER BY created_at ASC
CREATE INDEX idx_mr_reviews_mr ON mr_reviews(mr_id, created_at ASC);
