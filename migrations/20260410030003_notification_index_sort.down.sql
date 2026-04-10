DROP INDEX idx_notifications_user_status;
CREATE INDEX idx_notifications_user_status ON notifications(user_id, status);
