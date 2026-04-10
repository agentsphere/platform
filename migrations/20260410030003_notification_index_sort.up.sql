-- Replace (user_id, status) with (user_id, status, created_at DESC)
-- to cover both the filter and the ORDER BY in notification queries.
DROP INDEX idx_notifications_user_status;
CREATE INDEX idx_notifications_user_status
  ON notifications(user_id, status, created_at DESC);
