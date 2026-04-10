DROP TRIGGER IF EXISTS trg_api_tokens_updated_at ON api_tokens;
ALTER TABLE api_tokens DROP COLUMN updated_at;

DROP TRIGGER IF EXISTS trg_alert_rules_updated_at ON alert_rules;
ALTER TABLE alert_rules DROP COLUMN updated_at;

DROP TRIGGER IF EXISTS trg_webhooks_updated_at ON webhooks;
ALTER TABLE webhooks DROP COLUMN updated_at;
