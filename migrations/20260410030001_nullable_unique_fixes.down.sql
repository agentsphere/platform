ALTER TABLE user_roles
  DROP CONSTRAINT user_roles_user_id_role_id_project_id_key;
ALTER TABLE user_roles
  ADD CONSTRAINT user_roles_user_id_role_id_project_id_key
  UNIQUE (user_id, role_id, project_id);

ALTER TABLE delegations
  DROP CONSTRAINT uq_delegations_unique_grant;
ALTER TABLE delegations
  ADD CONSTRAINT delegations_delegator_id_delegate_id_permission_id_project__key
  UNIQUE (delegator_id, delegate_id, permission_id, project_id);
