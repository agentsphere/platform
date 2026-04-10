-- user_roles: fix nullable project_id in unique constraint.
-- NULLS NOT DISTINCT: (user_id, role_id, NULL) is now unique —
-- prevents granting the same global role to a user twice.
ALTER TABLE user_roles
  DROP CONSTRAINT user_roles_user_id_role_id_project_id_key;
ALTER TABLE user_roles
  ADD CONSTRAINT user_roles_user_id_role_id_project_id_key
  UNIQUE NULLS NOT DISTINCT (user_id, role_id, project_id);

-- delegations: fix nullable project_id in unique constraint.
-- Existing name is truncated by Postgres to 63 chars.
ALTER TABLE delegations
  DROP CONSTRAINT delegations_delegator_id_delegate_id_permission_id_project__key;
ALTER TABLE delegations
  ADD CONSTRAINT uq_delegations_unique_grant
  UNIQUE NULLS NOT DISTINCT (delegator_id, delegate_id, permission_id, project_id);
