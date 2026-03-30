/**
 * MCP Gate — Permission mode enforcement for manager agent sessions.
 *
 * Every MCP tool call passes through gate() which checks the current
 * permission mode and decides: auto (execute), ask (confirmation required),
 * or deny (plan mode).
 *
 * Approvals are stored in Valkey (via platform API), NOT in tool parameters.
 * The LLM cannot bypass the gate — approvals flow through UI -> backend -> Valkey.
 */

import crypto from 'node:crypto';
import { apiGet, apiPost } from './client.js';

const SESSION_ID = process.env.SESSION_ID || '';

// Action type classification for every known MCP tool.
// All tools from the 6 MCP servers must be listed here so the gate
// can make a deterministic decision.  Unknown tools fail closed ('ask').
export const ACTION_TYPES = {
  // ---- READ — auto in all modes ----
  // platform-core
  list_projects: 'READ', get_project: 'READ',
  get_session: 'READ', get_worker_progress: 'READ',
  list_children: 'READ', read_secret: 'READ',
  // platform-admin
  list_users: 'READ', get_user: 'READ',
  list_roles: 'READ', list_delegations: 'READ',
  // platform-pipeline
  list_pipelines: 'READ', get_pipeline: 'READ',
  get_step_logs: 'READ', list_artifacts: 'READ', download_artifact: 'READ',
  // platform-deploy
  list_targets: 'READ', get_target: 'READ',
  list_releases: 'READ', get_release: 'READ',
  release_history: 'READ', staging_status: 'READ',
  // platform-observe
  search_logs: 'READ', get_trace: 'READ', list_traces: 'READ',
  query_metrics: 'READ', list_metric_names: 'READ',
  list_alerts: 'READ', get_alert: 'READ',
  // platform-issues
  list_issues: 'READ', get_issue: 'READ', list_issue_comments: 'READ',
  list_merge_requests: 'READ', get_merge_request: 'READ',

  // ---- CREATE ----
  // platform-core
  create_project: 'CREATE', spawn_agent: 'CREATE', ask_for_secret: 'CREATE',
  // platform-admin
  create_user: 'CREATE', create_role: 'CREATE', create_delegation: 'CREATE',
  // platform-pipeline
  trigger_pipeline: 'CREATE',
  // platform-deploy
  create_target: 'CREATE', create_release: 'CREATE',
  // platform-issues
  create_issue: 'CREATE', add_issue_comment: 'CREATE',
  create_merge_request: 'CREATE', add_mr_comment: 'CREATE',

  // ---- UPDATE ----
  // platform-core
  update_project: 'UPDATE', send_message_to_session: 'UPDATE',
  // platform-admin
  update_user: 'UPDATE', assign_role: 'UPDATE',
  // platform-pipeline
  cancel_pipeline: 'UPDATE',
  // platform-deploy
  adjust_traffic: 'UPDATE',
  // platform-issues
  update_issue: 'UPDATE', update_merge_request: 'UPDATE',
  merge_mr: 'UPDATE',

  // ---- DELETE ----
  // platform-core
  delete_project: 'DELETE',
  // platform-admin
  deactivate_user: 'DELETE', remove_role: 'DELETE',
  revoke_delegation: 'DELETE',

  // ---- DEPLOY ----
  promote_release: 'DEPLOY',
  rollback_release: 'DEPLOY',
};

// Mode matrix: how each mode handles each action type
const MODE_MATRIX = {
  plan:       { READ: 'auto', CREATE: 'deny',  UPDATE: 'deny',  DELETE: 'deny',  DEPLOY: 'deny'  },
  guided:     { READ: 'auto', CREATE: 'ask',   UPDATE: 'ask',   DELETE: 'ask',   DEPLOY: 'ask'   },
  auto_read:  { READ: 'auto', CREATE: 'ask',   UPDATE: 'ask',   DELETE: 'ask',   DEPLOY: 'ask'   },
  auto_write: { READ: 'auto', CREATE: 'auto',  UPDATE: 'auto',  DELETE: 'ask',   DEPLOY: 'ask'   },
  full_auto:  { READ: 'auto', CREATE: 'auto',  UPDATE: 'auto',  DELETE: 'auto',  DEPLOY: 'auto'  },
};

/**
 * Determine the gate decision for a tool call.
 * @returns 'auto' | 'ask' | 'deny'
 */
export function gate(toolName, mode) {
  const actionType = ACTION_TYPES[toolName] || 'UNKNOWN';
  // Fail closed: unknown tools require confirmation in all modes except full_auto
  if (actionType === 'UNKNOWN') {
    return mode === 'full_auto' ? 'auto' : 'ask';
  }
  return MODE_MATRIX[mode]?.[actionType] || 'ask';
}

/**
 * Compute a deterministic hash for an action (session + tool + params).
 * Used as the Valkey key for approval state.
 */
export function computeActionHash(sessionId, toolName, params) {
  const payload = JSON.stringify({ s: sessionId, t: toolName, p: params });
  return crypto.createHash('sha256').update(payload).digest('hex').slice(0, 16);
}

/**
 * Read the current permission mode from Valkey (via platform API).
 * Falls back to 'auto_read' if not set.
 */
export async function readCurrentMode() {
  if (!SESSION_ID) return process.env.MANAGER_MODE || 'auto_read';
  try {
    const resp = await apiGet(`/api/manager/sessions/${SESSION_ID}`);
    return resp.mode || 'auto_read';
  } catch {
    return process.env.MANAGER_MODE || 'auto_read';
  }
}

/**
 * Check if a specific action hash has been approved in Valkey.
 */
export async function checkApproval(sessionId, actionHash) {
  try {
    const resp = await apiGet(`/api/manager/sessions/${sessionId}/approval/${actionHash}`);
    return resp.approved === true;
  } catch {
    return false;
  }
}

/**
 * Check if a tool name has been session-approved (for CREATE/UPDATE tools).
 */
export async function isToolSessionApproved(sessionId, toolName) {
  const actionType = ACTION_TYPES[toolName];
  // Only CREATE and UPDATE can be session-approved (never DELETE/DEPLOY)
  if (actionType !== 'CREATE' && actionType !== 'UPDATE') return false;
  try {
    const resp = await apiGet(`/api/manager/sessions/${sessionId}/approved_tools`);
    return (resp.tools || []).includes(toolName);
  } catch {
    return false;
  }
}

/**
 * Register a pending action and publish a confirmation_needed event to SSE.
 */
export async function setPending(sessionId, actionHash, summary, toolName) {
  try {
    await apiPost(`/api/manager/sessions/${sessionId}/pending_action`, {
      body: { action_hash: actionHash, summary, tool: toolName },
    });
  } catch (e) {
    console.error('Failed to set pending action:', e.message);
  }
}

/**
 * Wait for user approval of a pending action.
 *
 * Polls the approval endpoint every 1s for up to `timeoutSec` seconds.
 * Returns: 'approved' | 'rejected' | 'timeout'
 *
 * This is the synchronous confirmation flow:
 * - MCP gate registers pending action (which triggers SSE event → UI shows buttons)
 * - MCP gate polls for approval while Claude CLI waits for the tool result
 * - If user approves within timeout → return 'approved', gate executes the tool
 * - If timeout → return 'timeout', Claude is told to continue with other work
 * - If rejected → return 'rejected' with optional feedback
 */
export async function waitForApproval(sessionId, actionHash, timeoutSec = 10) {
  const start = Date.now();
  const deadline = start + timeoutSec * 1000;

  while (Date.now() < deadline) {
    try {
      // Check approval (consumed on read — single-use)
      const resp = await apiGet(`/api/manager/sessions/${sessionId}/approval/${actionHash}`);
      if (resp.approved === true) return 'approved';

      // Check rejection
      const rejResp = await apiGet(`/api/manager/sessions/${sessionId}/rejection/${actionHash}`).catch(() => null);
      if (rejResp?.rejected === true) return 'rejected';
    } catch {
      // API error — keep polling
    }

    // Wait 1s before next poll
    await new Promise(resolve => setTimeout(resolve, 1000));
  }

  return 'timeout';
}

/**
 * Full gate check with synchronous approval flow.
 *
 * Call this at the top of every MCP server's CallToolRequestSchema handler.
 * Returns null if the tool should execute, or a content response to return to Claude.
 *
 * Flow for 'ask' decisions:
 * 1. Check if already approved (session-level or action-level)
 * 2. If not, register pending action (publishes confirmation_needed SSE event)
 * 3. Poll for approval for 10 seconds
 * 4. If approved → return null (execute the tool)
 * 5. If timeout → tell Claude the action is pending user approval
 * 6. If rejected → tell Claude with optional feedback
 */
export async function gateCheck(sessionId, toolName, args) {
  if (!sessionId || process.env.MANAGER_MODE === undefined) {
    return null; // Not a manager session — skip gate
  }

  const mode = await readCurrentMode();
  const decision = gate(toolName, mode);

  if (decision === 'auto') return null; // Execute immediately

  if (decision === 'deny') {
    return {
      content: [{
        type: 'text',
        text: JSON.stringify({
          status: 'denied',
          tool: toolName,
          action_type: ACTION_TYPES[toolName] || 'UNKNOWN',
          reason: `Action "${toolName}" is not available in ${mode} mode. Do NOT attempt alternative write operations. Describe this as a numbered plan step instead.`,
        })
      }]
    };
  }

  // decision === 'ask'
  const actionHash = computeActionHash(sessionId, toolName, args);

  // Check if already approved (from a previous confirmation round)
  const alreadyApproved = await checkApproval(sessionId, actionHash);
  if (alreadyApproved) return null;

  // Check if this tool is session-approved
  const sessionApproved = await isToolSessionApproved(sessionId, toolName);
  if (sessionApproved) return null;

  // Register pending action (triggers SSE confirmation_needed event in UI)
  const summary = buildSummary(toolName, args);
  await setPending(sessionId, actionHash, summary, toolName);

  // Wait synchronously for user to approve/reject (10s timeout)
  const result = await waitForApproval(sessionId, actionHash, 10);

  if (result === 'approved') return null; // User approved — execute the tool

  if (result === 'rejected') {
    return {
      content: [{
        type: 'text',
        text: JSON.stringify({
          status: 'rejected',
          tool: toolName,
          summary,
          message: 'The user rejected this action.',
        })
      }]
    };
  }

  // Timeout — tell Claude the action is pending
  return {
    content: [{
      type: 'text',
      text: JSON.stringify({
        status: 'pending_approval',
        tool: toolName,
        action_hash: actionHash,
        summary,
        message: `Action "${toolName}" requires user approval. The user has not responded yet. You may continue with other tasks and retry this action later.`,
      })
    }]
  };
}

/**
 * Build a human-readable summary of a tool call for the confirmation dialog.
 */
export function buildSummary(toolName, params) {
  const pid = params.project_id ? ` (project: ${params.project_id.slice(0, 8)})` : '';
  switch (toolName) {
    // CREATE
    case 'create_project': return `Create project "${params.name}"`;
    case 'spawn_agent': return `Spawn dev agent${pid}`;
    case 'trigger_pipeline': return `Trigger pipeline on ${params.git_ref || 'default ref'}${pid}`;
    case 'create_issue': return `Create issue "${params.title}"${pid}`;
    case 'create_merge_request': return `Create MR "${params.title}" (${params.source_branch} -> ${params.target_branch})${pid}`;
    case 'add_issue_comment': return `Comment on issue #${params.number}${pid}`;
    case 'add_mr_comment': return `Comment on MR #${params.number}${pid}`;
    case 'create_user': return `Create user "${params.name}"`;
    case 'create_role': return `Create role "${params.name}"`;
    case 'create_delegation': return `Delegate "${params.permission}" to user`;
    case 'create_target': return `Create deploy target "${params.name}"${pid}`;
    case 'create_release': return `Create release for ${params.image_ref}${pid}`;
    case 'ask_for_secret': return `Request secret "${params.name}"${pid}`;
    // UPDATE
    case 'update_project': return `Update project${pid}`;
    case 'update_issue': return `Update issue #${params.number}${pid}`;
    case 'update_merge_request': return `Update MR #${params.number}${pid}`;
    case 'merge_mr': return `Merge MR #${params.number}${pid}`;
    case 'update_user': return `Update user ${params.user_id ? params.user_id.slice(0, 8) : ''}`;
    case 'assign_role': return `Assign role to user ${params.user_id ? params.user_id.slice(0, 8) : ''}`;
    case 'cancel_pipeline': return `Cancel pipeline${pid}`;
    case 'send_message_to_session': return `Send message to session ${params.session_id ? params.session_id.slice(0, 8) : ''}`;
    case 'adjust_traffic': return `Adjust traffic to ${params.weight}%${pid}`;
    // DELETE
    case 'delete_project': return `DELETE project${pid}`;
    case 'deactivate_user': return `Deactivate user ${params.user_id ? params.user_id.slice(0, 8) : ''}`;
    case 'remove_role': return `Remove role from user ${params.user_id ? params.user_id.slice(0, 8) : ''}`;
    case 'revoke_delegation': return `Revoke delegation ${params.delegation_id ? params.delegation_id.slice(0, 8) : ''}`;
    // DEPLOY
    case 'promote_release': return `Promote release${pid}`;
    case 'rollback_release': return `Rollback release${pid}`;
    default: return `${toolName}${pid}`;
  }
}
