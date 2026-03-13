// Re-export all generated types from ts-rs.
// Run `just types` after changing any Rust response struct to regenerate.

// Auth / Users
export type { User } from './generated/User';
export type { UserType } from './generated/UserType';
export type { LoginResponse } from './generated/LoginResponse';
export type { ApiToken } from './generated/ApiToken';
export type { CreateTokenResponse } from './generated/CreateTokenResponse';

// Projects
export type { Project } from './generated/Project';

// Issues & Comments
export type { Issue } from './generated/Issue';
export type { Comment } from './generated/Comment';

// Merge Requests
export type { MergeRequest } from './generated/MergeRequest';
export type { Review } from './generated/Review';

// Pipelines
export type { Pipeline } from './generated/Pipeline';
export type { PipelineDetail } from './generated/PipelineDetail';
export type { PipelineStep } from './generated/PipelineStep';
export type { Artifact } from './generated/Artifact';

// Deployments
export type { Deployment } from './generated/Deployment';
export type { DeploymentHistory } from './generated/DeploymentHistory';
export type { OpsRepo } from './generated/OpsRepo';
export type { PreviewDeployment } from './generated/PreviewDeployment';

// Git Browser
export type { TreeEntry } from './generated/TreeEntry';
export type { BlobResponse } from './generated/BlobResponse';
export type { BranchInfo } from './generated/BranchInfo';
export type { CommitInfo } from './generated/CommitInfo';

// Admin
export type { Role } from './generated/Role';
export type { Permission } from './generated/Permission';
export type { Delegation } from './generated/Delegation';
export type { ServiceAccountResponse } from './generated/ServiceAccountResponse';

// Webhooks
export type { Webhook } from './generated/Webhook';

// Observability — Logs & Traces
export type { LogEntry } from './generated/LogEntry';
export type { TraceSummary } from './generated/TraceSummary';
export type { TraceDetail } from './generated/TraceDetail';
export type { Span } from './generated/Span';

// Observability — Metrics
export type { MetricDataPoint } from './generated/MetricDataPoint';
export type { MetricSeries } from './generated/MetricSeries';

// Observability — Alerts
export type { AlertRule } from './generated/AlertRule';
export type { AlertEvent } from './generated/AlertEvent';

// Agent Sessions
export type { AgentSession } from './generated/AgentSession';
export type { SessionDetail } from './generated/SessionDetail';
export type { SessionMessage } from './generated/SessionMessage';

// Notifications
export type { Notification } from './generated/Notification';
export type { UnreadCountResponse } from './generated/UnreadCountResponse';

// Workspaces
export type { Workspace } from './generated/Workspace';
export type { WorkspaceMember } from './generated/WorkspaceMember';

// Secrets
export type { Secret } from './generated/Secret';

// Passkeys
export type { PasskeyResponse } from './generated/PasskeyResponse';
export type { BeginLoginResponse } from './generated/BeginLoginResponse';
export type { PasskeyLoginResponse } from './generated/PasskeyLoginResponse';

// Dashboard
export type { DashboardStats } from './generated/DashboardStats';
export type { AuditLogEntry } from './generated/AuditLogEntry';
export type { OnboardingStatus } from './generated/OnboardingStatus';

// Validation
export type { ValidateKeyResponse } from './generated/ValidateKeyResponse';

// Pagination (also re-exported from api.ts)
export type { ListResponse } from './generated/ListResponse';

// --- Manual types (not backed by a Rust struct) ---

// WebSocket progress events from agent sessions
export interface ProgressEvent {
  kind: 'Thinking' | 'ToolCall' | 'ToolResult' | 'Milestone' | 'Error' | 'Completed' | 'WaitingForInput' | 'Text' | 'SecretRequest' | 'IframeAvailable' | 'IframeRemoved';
  message: string;
  metadata?: Record<string, any>;
}

// Iframe panel info returned by GET /api/projects/{id}/sessions/{sessionId}/iframes
export interface IframePanel {
  service_name: string;
  port: number;
  port_name: string;
  preview_url: string;
}

// Secret request metadata (within ProgressEvent.metadata for kind='SecretRequest')
export interface SecretRequestMeta {
  request_id: string;
  name: string;
  prompt: string;
  environments?: string[];
}
