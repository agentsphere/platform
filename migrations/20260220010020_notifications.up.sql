CREATE TABLE notifications (
    id                UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id           UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    notification_type TEXT NOT NULL,
    subject           TEXT NOT NULL,
    body              TEXT,
    channel           TEXT NOT NULL DEFAULT 'in_app'
                      CHECK (channel IN ('in_app', 'email', 'webhook')),
    status            TEXT NOT NULL DEFAULT 'pending'
                      CHECK (status IN ('pending', 'sent', 'read', 'failed')),
    ref_type          TEXT,
    ref_id            UUID,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_notifications_user_status ON notifications(user_id, status);
