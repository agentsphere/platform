-- Plan 44: Branch protection rules
CREATE TABLE branch_protection_rules (
    id                    UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id            UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    pattern               TEXT NOT NULL,
    require_pr            BOOLEAN NOT NULL DEFAULT true,
    block_force_push      BOOLEAN NOT NULL DEFAULT true,
    required_approvals    INTEGER NOT NULL DEFAULT 0 CHECK (required_approvals >= 0),
    dismiss_stale_reviews BOOLEAN NOT NULL DEFAULT true,
    required_checks       TEXT[] NOT NULL DEFAULT '{}',
    require_up_to_date    BOOLEAN NOT NULL DEFAULT false,
    allow_admin_bypass    BOOLEAN NOT NULL DEFAULT false,
    merge_methods         TEXT[] NOT NULL DEFAULT '{merge}',
    created_at            TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at            TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (project_id, pattern)
);
