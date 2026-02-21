// Auth / Users
export interface User {
  id: string;
  name: string;
  display_name: string | null;
  email: string;
  is_active: boolean;
  created_at: string;
  updated_at: string;
}

export interface LoginResponse {
  token: string;
  expires_at: string;
  user: User;
}

// Projects
export interface Project {
  id: string;
  owner_id: string;
  name: string;
  display_name: string | null;
  description: string | null;
  visibility: string;
  default_branch: string;
  is_active: boolean;
  created_at: string;
  updated_at: string;
}

// Issues
export interface Issue {
  id: string;
  project_id: string;
  number: number;
  author_id: string;
  title: string;
  body: string | null;
  status: string;
  labels: string[];
  assignee_id: string | null;
  created_at: string;
  updated_at: string;
}

// Comments (shared by issues + MRs)
export interface Comment {
  id: string;
  author_id: string;
  body: string;
  created_at: string;
  updated_at: string;
}

// Merge Requests
export interface MergeRequest {
  id: string;
  project_id: string;
  number: number;
  author_id: string;
  source_branch: string;
  target_branch: string;
  title: string;
  body: string | null;
  status: string;
  merged_by: string | null;
  merged_at: string | null;
  created_at: string;
  updated_at: string;
}

export interface Review {
  id: string;
  mr_id: string;
  reviewer_id: string;
  verdict: string;
  body: string | null;
  created_at: string;
}

// Pipelines
export interface Pipeline {
  id: string;
  project_id: string;
  trigger: string;
  git_ref: string;
  commit_sha: string | null;
  status: string;
  triggered_by: string | null;
  started_at: string | null;
  finished_at: string | null;
  created_at: string;
}

export interface PipelineStep {
  id: string;
  step_order: number;
  name: string;
  image: string;
  status: string;
  exit_code: number | null;
  duration_ms: number | null;
  log_ref: string | null;
  created_at: string;
}

export interface PipelineDetail extends Pipeline {
  steps: PipelineStep[];
}

export interface Artifact {
  id: string;
  name: string;
  content_type: string | null;
  size_bytes: number | null;
  expires_at: string | null;
  created_at: string;
}

// Deployments
export interface Deployment {
  id: string;
  project_id: string;
  environment: string;
  image_ref: string;
  desired_status: string;
  current_status: string;
  current_sha: string | null;
  deployed_by: string | null;
  deployed_at: string | null;
  created_at: string;
  updated_at: string;
}

// Git Browser
export interface TreeEntry {
  name: string;
  entry_type: string;
  mode: string;
  size: number | null;
  sha: string;
}

export interface BlobResponse {
  path: string;
  size: number;
  content: string;
  encoding: string;
}

export interface BranchInfo {
  name: string;
  sha: string;
  updated_at: string;
}

export interface CommitInfo {
  sha: string;
  message: string;
  author_name: string;
  author_email: string;
  authored_at: string;
}

// Admin
export interface Role {
  id: string;
  name: string;
  description: string | null;
  is_system: boolean;
  created_at: string;
}

export interface Permission {
  id: string;
  name: string;
  resource: string;
  action: string;
  description: string | null;
}

export interface Delegation {
  id: string;
  delegator_id: string;
  delegate_id: string;
  permission: string;
  project_id: string | null;
  expires_at: string | null;
  reason: string | null;
  created_at: string;
}

// Tokens
export interface ApiToken {
  id: string;
  name: string;
  scopes: string[];
  project_id: string | null;
  last_used_at: string | null;
  expires_at: string | null;
  created_at: string;
}

export interface CreateTokenResponse extends ApiToken {
  token: string;
}

// Webhooks
export interface Webhook {
  id: string;
  project_id: string;
  url: string;
  events: string[];
  active: boolean;
  created_at: string;
}

// Observability — Logs
export interface LogEntry {
  id: string;
  timestamp: string;
  trace_id?: string;
  span_id?: string;
  project_id?: string;
  session_id?: string;
  service: string;
  level: string;
  message: string;
  attributes?: Record<string, any>;
}

// Observability — Traces
export interface TraceSummary {
  trace_id: string;
  root_span: string;
  service: string;
  status: string;
  duration_ms?: number;
  started_at: string;
  project_id?: string;
}

export interface Span {
  span_id: string;
  parent_span_id?: string;
  name: string;
  service: string;
  kind: string;
  status: string;
  duration_ms?: number;
  started_at: string;
  finished_at?: string;
  attributes?: Record<string, any>;
  events?: SpanEvent[];
}

export interface SpanEvent {
  name: string;
  timestamp: string;
  attributes?: Record<string, any>;
}

// Observability — Metrics
export interface MetricDataPoint {
  timestamp: string;
  value: number;
}

export interface MetricSeries {
  name: string;
  labels: Record<string, string>;
  points: MetricDataPoint[];
}

// Observability — Alerts
export interface AlertRule {
  id: string;
  project_id?: string;
  name: string;
  query: string;
  condition: string;
  threshold: number;
  window_seconds: number;
  channels: string[];
  enabled: boolean;
  created_at: string;
}

export interface AlertEvent {
  id: string;
  alert_rule_id: string;
  status: string;
  value: number;
  message: string;
  created_at: string;
}

// Agent Sessions
export interface AgentSession {
  id: string;
  project_id: string;
  user_id: string;
  agent_user_id?: string;
  prompt: string;
  status: 'pending' | 'running' | 'completed' | 'failed' | 'stopped';
  branch?: string;
  pod_name?: string;
  provider: string;
  provider_config?: Record<string, any>;
  cost_tokens?: number;
  created_at: string;
  updated_at: string;
}

export interface ProgressEvent {
  kind: 'Thinking' | 'ToolCall' | 'ToolResult' | 'Milestone' | 'Error' | 'Completed' | 'Text';
  message: string;
  metadata?: Record<string, any>;
}

// Notifications
export interface Notification {
  id: string;
  notification_type: string;
  subject: string;
  body?: string;
  channel: string;
  status: string;
  ref_type?: string;
  ref_id?: string;
  created_at: string;
}

// Preview Deployments
export interface PreviewDeployment {
  id: string;
  project_id: string;
  branch: string;
  branch_slug: string;
  image_ref: string;
  desired_status: string;
  current_status: string;
  ttl_hours: number;
  expires_at: string;
  created_at: string;
}

// Audit Log
export interface AuditLogEntry {
  id: string;
  actor_id: string;
  actor_name: string;
  action: string;
  resource: string;
  resource_id?: string;
  project_id?: string;
  detail?: Record<string, any>;
  ip_addr?: string;
  created_at: string;
}

// Secrets
export interface Secret {
  id: string;
  project_id: string;
  name: string;
  scope: string;
  version: number;
  created_at: string;
  updated_at: string;
}
