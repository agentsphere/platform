# Plan 23 — Web UI Completion

## Overview

Extend the existing Preact SPA skeleton into a fully functional dashboard. The current UI has basic page shells for projects, issues, MRs, and admin, but is missing several key pages and many existing pages need polish: observability dashboards (logs, traces, metrics), agent session viewer with live streaming, deployment dashboard, and enhanced admin panel.

---

## Current State

### What Exists (~1.5K TS/TSX)

| Component | Status | Lines |
|---|---|---|
| `index.tsx` — Router + auth guard | Complete | 52 |
| `lib/api.ts` — HTTP client | Complete | ~50 |
| `lib/auth.tsx` — Auth context | Complete | ~40 |
| `lib/format.ts` — Date formatting | Complete | ~30 |
| `lib/types.ts` — TypeScript interfaces | Complete | ~120 |
| `components/Layout.tsx` — Sidebar nav | Complete | ~60 |
| `components/Badge.tsx` — Status badges | Minimal | ~10 |
| `components/Markdown.tsx` — MD renderer | Minimal | ~10 |
| `components/Modal.tsx` — Dialog | Complete | ~30 |
| `components/Pagination.tsx` — Page nav | Complete | ~25 |
| `pages/Login.tsx` — Login form | Complete | 52 |
| `pages/Dashboard.tsx` — Project list | Basic | 52 |
| `pages/Projects.tsx` — Project browser | Complete | 104 |
| `pages/ProjectDetail.tsx` — 7-tab detail | Complete | 495 |
| `pages/IssueDetail.tsx` — Issue + comments | Complete | 94 |
| `pages/MRDetail.tsx` — MR + reviews | Complete | 146 |
| `pages/PipelineDetail.tsx` — Steps + logs | Complete | 129 |
| `pages/admin/Users.tsx` — User mgmt | Basic | 104 |
| `pages/admin/Roles.tsx` — Role editor | Basic | 151 |
| `pages/admin/Delegations.tsx` — Delegation mgmt | Basic | 139 |
| `pages/admin/Tokens.tsx` — Token mgmt | Basic | 138 |
| `style.css` — Dark theme | Complete | ~250 |

### What's Missing

1. **Observability pages**: Log search, trace waterfall, metric charts, alerts
2. **Agent session viewer**: Live streaming, chat interface, session list
3. **Deployment dashboard**: Environment status grid, history, rollback controls
4. **Preview environment page**: Preview list per project, status, cleanup
5. **Dashboard enhancements**: Activity feed, status overview cards
6. **Notification bell**: Unread badge, notification dropdown
7. **WebSocket integration**: Live streaming for logs, agent sessions
8. **Shared components**: Table, TimeAgo, CodeBlock, Toast

---

## Prerequisites

| Requirement | Status |
|---|---|
| Preact + esbuild + preact-router | Complete |
| API client with auth | Complete |
| Backend APIs for all modules | Complete |
| `marked` for markdown | Already in package.json |
| WebSocket endpoints | Complete (agent streaming, log tailing) |

### New UI Dependencies

| Package | Purpose | Size |
|---|---|---|
| `uplot` | Lightweight time-series charts (metrics page) | ~35KB min |
| `highlight.js` | Code syntax highlighting (file browser) | ~30KB (core + few langs) |

Add to `ui/package.json`:
```json
{
  "dependencies": {
    "uplot": "^1.6",
    "highlight.js": "^11.10"
  }
}
```

---

## Implementation Phases

### Phase U1: Shared Foundation (~400 LOC)

#### U1.1: WebSocket Client (`ui/src/lib/ws.ts`)

```typescript
interface WsOptions {
  url: string;
  onMessage: (data: any) => void;
  onOpen?: () => void;
  onClose?: () => void;
  onError?: (err: Event) => void;
  reconnect?: boolean;       // default true
  maxRetries?: number;       // default 5
}

class ReconnectingWebSocket {
  private ws: WebSocket | null = null;
  private retries = 0;
  private backoff = 1000;  // start at 1s, max 30s

  connect(): void { /* ... */ }
  send(data: string): void { /* ... */ }
  close(): void { /* ... */ }
}

export function createWs(options: WsOptions): ReconnectingWebSocket;
```

Features:
- Auto-reconnect with exponential backoff (1s, 2s, 4s, 8s, 16s, 30s cap)
- JSON message parsing
- Graceful close on component unmount
- Session cookie sent automatically (`credentials: include` equivalent)

#### U1.2: Extended Type Definitions (`ui/src/lib/types.ts`)

Add missing types:

```typescript
// Observability
interface LogEntry {
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

interface TraceSummary {
  trace_id: string;
  root_span: string;
  service: string;
  status: string;
  duration_ms?: number;
  started_at: string;
  project_id?: string;
}

interface Span {
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

interface SpanEvent {
  name: string;
  timestamp: string;
  attributes?: Record<string, any>;
}

// Agent sessions
interface AgentSession {
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

interface ProgressEvent {
  kind: 'Thinking' | 'ToolCall' | 'ToolResult' | 'Milestone' | 'Error' | 'Completed' | 'Text';
  message: string;
  metadata?: Record<string, any>;
}

// Notifications
interface Notification {
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

// Previews
interface PreviewDeployment {
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

// Metric types
interface MetricDataPoint {
  timestamp: string;
  value: number;
}

interface MetricSeries {
  name: string;
  labels: Record<string, string>;
  points: MetricDataPoint[];
}

// Alert types
interface AlertRule {
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
```

#### U1.3: Shared UI Components

**`ui/src/components/Table.tsx`** (~60 lines)
- Sortable column headers
- Striped rows
- Loading skeleton state
- Empty state message

**`ui/src/components/TimeAgo.tsx`** (~20 lines)
- Relative time display ("2 minutes ago", "3 hours ago")
- Tooltip with absolute time

**`ui/src/components/CodeBlock.tsx`** (~30 lines)
- Syntax highlighting via `highlight.js`
- Line numbers
- Copy button

**`ui/src/components/Toast.tsx`** (~40 lines)
- Stack-based notification system
- Auto-dismiss after 5 seconds
- Success/error/info variants

**`ui/src/components/StatusDot.tsx`** (~15 lines)
- Colored dot indicator (green=healthy, yellow=pending, red=failed)

**`ui/src/components/FilterBar.tsx`** (~50 lines)
- Horizontal bar of select/input filters
- "Apply" button
- URL query param sync

---

### Phase U2: Observability Pages (~800 LOC)

#### U2.1: Log Search Page (`ui/src/pages/observe/Logs.tsx`, ~200 lines)

**Route**: `/observe/logs`

**Layout**:
```
┌────────────────────────────────────────────┐
│ [Time range: ▼ Last 1h] [Level: ▼ All]    │
│ [Service: ▼ All] [Project: ▼ All]         │
│ [Search: ________________] [🔍 Search]     │
│ [□ Live tail]                              │
├────────────────────────────────────────────┤
│ 14:32:01 INFO  platform   User logged in  │
│ 14:32:00 ERROR pipeline   Step failed...  │
│ 14:31:58 WARN  deployer   Timeout wait... │
│ 14:31:55 DEBUG agent      Tool call: ed.. │
│ ...                                        │
│ ▾ Expand row → full attributes JSON       │
├────────────────────────────────────────────┤
│ [Pagination: 1 2 3 ... 10]                │
└────────────────────────────────────────────┘
```

**Features**:
- Filter by: time range, log level, service, project, session, full-text query
- Expandable rows showing attributes JSON
- Live tail toggle via WebSocket
- Link trace_id to trace detail page
- Color-coded log levels (red=error, yellow=warn, blue=info, gray=debug)

**API calls**:
- `GET /api/observe/logs` with query params
- WebSocket `/api/observe/logs/tail` for live streaming

#### U2.2: Trace List + Waterfall (`ui/src/pages/observe/Traces.tsx`, ~250 lines)

**Route**: `/observe/traces` (list), `/observe/traces/:traceId` (detail)

**List view**:
```
┌──────────────────────────────────────────────┐
│ [Service: ▼ All] [Status: ▼ All] [Time: ▼]  │
├──────────────────────────────────────────────┤
│ Trace ID  │ Root Span      │ Duration │ Stat │
│ abc123... │ POST /api/proj │ 45ms     │ ✓ OK │
│ def456... │ git-push       │ 2.3s     │ ✗ Err│
│ ghi789... │ pipeline.exec  │ 45s      │ ✓ OK │
└──────────────────────────────────────────────┘
```

**Waterfall detail view**:
```
┌─────────────────────────────────────────────────┐
│ Trace: abc123def456...                          │
│ Total: 45ms | Spans: 8 | Service: platform     │
├─────────────────────────────────────────────────┤
│ ├─ POST /api/projects/create     ████████ 45ms  │
│ │  ├─ auth.validate_token        ██ 2ms         │
│ │  ├─ rbac.check_permission      ███ 5ms        │
│ │  └─ db.insert_project          █████ 12ms     │
│ │     └─ sqlx.query              ████ 10ms      │
│                                                  │
│ [Click span for attributes + events]            │
├─────────────────────────────────────────────────┤
│ Selected Span: db.insert_project                │
│ Service: platform | Kind: internal | Status: ok │
│ Attributes: { db.statement: "INSERT..." }       │
│ Events: [{ name: "row_returned", ts: "..." }]   │
└─────────────────────────────────────────────────┘
```

**Features**:
- Horizontal bar chart showing span timeline relative to trace start
- Nested spans with indentation matching parent-child relationships
- Click span to show attributes/events panel
- Color by service or status
- Link to related logs (filter by trace_id)

#### U2.3: Metrics Charts (`ui/src/pages/observe/Metrics.tsx`, ~200 lines)

**Route**: `/observe/metrics`

**Layout**:
```
┌──────────────────────────────────────────────┐
│ [Metric: ▼ http_requests_total]              │
│ [Labels: method=POST, status=200]            │
│ [Time range: ▼ Last 1h]                     │
├──────────────────────────────────────────────┤
│                                              │
│  ╭──────────────────────────────────────╮    │
│  │ 150 ┤          ╭──╮                  │    │
│  │ 100 ┤   ╭──────╯  ╰────╮            │    │
│  │  50 ┤───╯               ╰─────╮     │    │
│  │   0 ┤                         ╰──── │    │
│  │     14:00  14:15  14:30  14:45 15:00 │    │
│  ╰──────────────────────────────────────╯    │
│                                              │
│ [Add series] [Remove series]                 │
└──────────────────────────────────────────────┘
```

**Features**:
- Metric name selector (populated from `GET /api/observe/metrics/names`)
- Label filter builder
- Time range selector (1h, 6h, 24h, 7d, custom)
- `uplot` line chart with multiple series support
- Auto-refresh every 30 seconds (configurable)

#### U2.4: Alerts Page (`ui/src/pages/observe/Alerts.tsx`, ~150 lines)

**Route**: `/observe/alerts`

**Features**:
- Alert rule list with enabled/disabled toggle
- Create/edit alert rule form (name, query, condition, threshold, channels)
- Alert history timeline (firing/resolved)
- Acknowledge/silence actions

---

### Phase U3: Agent Session Viewer (~400 LOC)

#### U3.1: Session List (`ui/src/pages/Sessions.tsx`, ~100 lines)

**Route**: `/projects/:id/sessions` (tab) or `/sessions` (global)

**Features**:
- Session list with status badges, duration, prompt preview
- Filter by status (running, completed, failed, stopped)
- "New Session" button opening creation modal

#### U3.2: Session Detail + Live Streaming (`ui/src/pages/SessionDetail.tsx`, ~300 lines)

**Route**: `/projects/:id/sessions/:sessionId`

**Layout**:
```
┌────────────────────────────────────────────────────┐
│ Session: abc123... | Status: Running ● | 5m 23s    │
│ Project: my-app | Branch: agent/abc123 | Cost: 12K │
├──────────────────────────────────────┬─────────────┤
│ [Live Output]                        │ [Session    │
│                                      │  Info]      │
│ 🤔 Thinking...                       │             │
│   Analyzing the codebase structure   │ Prompt:     │
│                                      │ "Add login  │
│ 🔧 Tool: Edit file                   │  page"     │
│   src/pages/Login.tsx                │             │
│   +import { useState } from...       │ Provider:   │
│   +export function Login() {         │ claude-code │
│                                      │             │
│ 🔧 Tool: Bash                        │ Role: dev   │
│   $ npm run build                    │             │
│   > Build succeeded                  │ Tokens:     │
│                                      │ 12,345      │
│ ✅ Milestone: Login page created     │             │
│                                      │             │
├──────────────────────────────────────┤             │
│ [Send message: _______________] [⏎]  │ [🛑 Stop]  │
└──────────────────────────────────────┴─────────────┘
```

**Features**:
- **Real-time streaming**: WebSocket connection to agent log stream
  - Progress events rendered as timeline
  - Tool calls highlighted with collapsible detail (file edits, bash commands)
  - Thinking/reasoning in collapsible blocks
  - Milestones as green checkmarks
  - Errors in red
- **Chat input**: Send messages to running agent via `send_message` API
- **Session metadata sidebar**: Project, branch, provider, role, token cost, duration
- **Stop button**: Terminate running session
- **Historical view**: For completed sessions, load logs from MinIO

**WebSocket protocol**:
```javascript
// Connect to agent log stream
const ws = createWs({
  url: `/api/projects/${projectId}/sessions/${sessionId}/ws`,
  onMessage: (event: ProgressEvent) => {
    appendEvent(event);  // Add to event timeline
  }
});
```

**Event rendering**:
```typescript
function renderEvent(event: ProgressEvent) {
  switch (event.kind) {
    case 'Thinking':
      return <div class="event thinking">🤔 {event.message}</div>;
    case 'ToolCall':
      return <div class="event tool-call">🔧 {event.message}</div>;
    case 'ToolResult':
      return <div class="event tool-result">{event.message}</div>;
    case 'Milestone':
      return <div class="event milestone">✅ {event.message}</div>;
    case 'Error':
      return <div class="event error">❌ {event.message}</div>;
    case 'Completed':
      return <div class="event completed">🎉 {event.message}</div>;
    case 'Text':
      return <div class="event text">{event.message}</div>;
  }
}
```

---

### Phase U4: Deployment Dashboard (~300 LOC)

#### U4.1: Deployment Overview (`ui/src/pages/Deployments.tsx`, ~150 lines)

Embedded in `ProjectDetail.tsx` deployments tab, enhanced:

**Layout**:
```
┌────────────────────────────────────────────────────┐
│ Environments                                       │
├────────────────────────────────────────────────────┤
│ ┌──────────────┐ ┌──────────────┐ ┌──────────────┐│
│ │ Production   │ │ Staging      │ │ Preview      ││
│ │ ● Healthy    │ │ ● Syncing    │ │ ○ 3 active   ││
│ │ v1.2.3       │ │ v1.3.0-rc1  │ │              ││
│ │ 2h ago       │ │ 5m ago       │ │              ││
│ │ [Rollback]   │ │              │ │ [View all]   ││
│ └──────────────┘ └──────────────┘ └──────────────┘│
├────────────────────────────────────────────────────┤
│ Deployment History (Production)                    │
│ ┌──────┬────────┬──────────┬────────┬────────────┐│
│ │ Time │ Image  │ Action   │ Status │ Deployed By││
│ │ 2h   │ v1.2.3 │ deploy   │ ✓      │ ci-bot     ││
│ │ 1d   │ v1.2.2 │ deploy   │ ✓      │ ci-bot     ││
│ │ 2d   │ v1.2.1 │ rollback │ ✓      │ admin      ││
│ └──────┴────────┴──────────┴────────┴────────────┘│
└────────────────────────────────────────────────────┘
```

**Features**:
- Environment status cards (production, staging, preview)
- Deployment history table per environment
- Rollback button (with confirmation modal)
- Real-time status updates via polling (10s interval)

#### U4.2: Preview Environments List (~100 lines)

Part of deployments tab:

```
┌────────────────────────────────────────────────────┐
│ Active Previews                                    │
├────────┬──────────┬──────────┬──────────┬─────────┤
│ Branch │ Status   │ Image    │ Expires  │ Actions │
│ feat/x │ ● Health │ v0.1-abc │ 20h left │ [Del]   │
│ fix/y  │ ◐ Sync   │ v0.1-def │ 23h left │ [Del]   │
│ ui/z   │ ✗ Failed │ v0.1-ghi │ 12h left │ [Del]   │
└────────┴──────────┴──────────┴──────────┴─────────┘
```

---

### Phase U5: Dashboard Enhancement (~200 LOC)

#### U5.1: Activity Feed + Status Cards

**Route**: `/` (Dashboard)

**Layout**:
```
┌─────────────────────────────────────────────────┐
│ Platform Dashboard                               │
├──────────┬──────────┬──────────┬────────────────┤
│ Projects │ Sessions │ Builds   │ Deployments    │
│   12     │ 3 active │ 2 running│ 4 healthy      │
│          │          │ 1 failed │ 1 degraded     │
├──────────┴──────────┴──────────┴────────────────┤
│ Recent Activity                                  │
│ • admin created project "my-app"      2m ago    │
│ • ci-bot pipeline #42 succeeded       5m ago    │
│ • agent-abc1 session completed        10m ago   │
│ • admin assigned developer role       15m ago   │
│ ...                                              │
├──────────────────────────────────────────────────┤
│ Quick Actions                                    │
│ [New Project] [New Session] [View Logs]          │
└──────────────────────────────────────────────────┘
```

**Features**:
- Status summary cards (counts from various APIs)
- Recent audit log activity feed
- Quick action buttons

#### U5.2: Notification Bell (`ui/src/components/NotificationBell.tsx`, ~50 lines)

```
┌──────────────────────────────┐
│ 🔔 (3)                       │  ← Unread count badge
├──────────────────────────────┤
│ Build #42 failed      2m ago │  ← Dropdown on click
│ Deployment healthy    5m ago │
│ New issue #15        10m ago │
│ ─────────────────────────── │
│ [View all notifications]     │
└──────────────────────────────┘
```

- Polls `GET /api/notifications/unread-count` every 30s
- Dropdown shows latest 5 notifications
- Click notification → navigate to referenced resource
- "Mark as read" on click

---

### Phase U6: Polish & Missing Features (~300 LOC)

#### U6.1: Secret Management UI Enhancement

Currently shown in project settings tab. Enhance:
- Show secret metadata (name, scope, version, updated_at)
- Create secret form (name, value, scope)
- Warning: "Secret values are never displayed after creation"
- Delete confirmation modal

#### U6.2: Token Creation "Copy Once" UX

When creating API tokens:
- Show raw token in a highlighted box
- "Copy to clipboard" button
- Warning: "This token will not be shown again"
- After dismissing modal, token value is gone

#### U6.3: Responsive Sidebar

- Collapsible on narrow screens
- Hamburger menu toggle
- Active route highlighting

#### U6.4: Error Boundary

```typescript
class ErrorBoundary extends Component {
  componentDidCatch(error: Error) {
    // Show friendly error message
    // Log to console for debugging
    // "Something went wrong" with retry button
  }
}
```

---

## Routing (Complete)

```typescript
<Router>
  {/* Auth */}
  <Login path="/login" />

  {/* Dashboard */}
  <Dashboard path="/" />

  {/* Projects */}
  <Projects path="/projects" />
  <ProjectDetail path="/projects/:id/:tab?" />
  <IssueDetail path="/projects/:id/issues/:number" />
  <MRDetail path="/projects/:id/merge-requests/:number" />
  <PipelineDetail path="/projects/:id/pipelines/:pipelineId" />
  <SessionDetail path="/projects/:id/sessions/:sessionId" />

  {/* Observability */}
  <Logs path="/observe/logs" />
  <Traces path="/observe/traces" />
  <TraceDetail path="/observe/traces/:traceId" />
  <Metrics path="/observe/metrics" />
  <Alerts path="/observe/alerts" />

  {/* Admin */}
  <Users path="/admin/users" />
  <Roles path="/admin/roles" />
  <Delegations path="/admin/delegations" />
  <Tokens path="/settings/tokens" />
</Router>
```

---

## Files Changed

### New Files

| File | Phase | Lines |
|------|-------|-------|
| `ui/src/lib/ws.ts` | U1 | ~60 |
| `ui/src/components/Table.tsx` | U1 | ~60 |
| `ui/src/components/TimeAgo.tsx` | U1 | ~20 |
| `ui/src/components/CodeBlock.tsx` | U1 | ~30 |
| `ui/src/components/Toast.tsx` | U1 | ~40 |
| `ui/src/components/StatusDot.tsx` | U1 | ~15 |
| `ui/src/components/FilterBar.tsx` | U1 | ~50 |
| `ui/src/components/NotificationBell.tsx` | U5 | ~50 |
| `ui/src/pages/observe/Logs.tsx` | U2 | ~200 |
| `ui/src/pages/observe/Traces.tsx` | U2 | ~250 |
| `ui/src/pages/observe/Metrics.tsx` | U2 | ~200 |
| `ui/src/pages/observe/Alerts.tsx` | U2 | ~150 |
| `ui/src/pages/SessionDetail.tsx` | U3 | ~300 |
| `ui/src/pages/Sessions.tsx` | U3 | ~100 |

### Modified Files

| File | Phase | Changes |
|------|-------|---------|
| `ui/src/lib/types.ts` | U1 | Add observability, session, notification, preview types |
| `ui/src/components/Layout.tsx` | U5 | Add notification bell, observability nav section |
| `ui/src/index.tsx` | All | Add new routes |
| `ui/src/pages/Dashboard.tsx` | U5 | Status cards, activity feed |
| `ui/src/pages/ProjectDetail.tsx` | U4 | Enhanced deployments tab with previews |
| `ui/src/style.css` | All | Styles for new components |
| `ui/package.json` | U2 | Add uplot, highlight.js |

---

## Implementation Sequence

| Phase | Scope | Estimated LOC | Dependencies |
|-------|-------|---------------|-------------|
| **U1** | Shared foundation (ws, types, components) | ~400 | None |
| **U2** | Observability pages (logs, traces, metrics, alerts) | ~800 | U1 |
| **U3** | Agent session viewer (list + live streaming) | ~400 | U1 |
| **U4** | Deployment dashboard (environments, previews) | ~300 | U1, Plan 20 (preview API) |
| **U5** | Dashboard enhancement (activity, notifications) | ~200 | U1 |
| **U6** | Polish (secrets UX, tokens UX, responsive, errors) | ~300 | U1-U5 |

**U1 must come first. U2-U5 can be parallelized. U6 last.**

---

## Security Considerations

- **XSS prevention**: Never use `dangerouslySetInnerHTML` with raw user input. Use `marked` with sanitization for markdown.
- **CSP**: Add `<meta http-equiv="Content-Security-Policy" content="default-src 'self'; style-src 'self' 'unsafe-inline'">` to prevent script injection
- **Secret values**: Never displayed in the UI after creation
- **Token display**: Show once, then gone forever — clear UI warning
- **WebSocket auth**: Connections include session cookie; handle 401 reconnection
- **Error messages**: Don't expose internal server errors to UI; map 500 to "Something went wrong"
- **Input validation**: Client-side mirrors server-side limits for UX, but server is source of truth

---

## Verification

### Per Phase
1. `cd ui && npm run build` — build succeeds
2. `just build` — full platform build with embedded UI
3. Manual testing: navigate to each new page, verify data loads

### Final
1. All pages render without console errors
2. WebSocket streaming works for logs and agent sessions
3. Trace waterfall displays correctly with nested spans
4. Metrics charts render with real data
5. Notification bell shows correct unread count
6. Admin pages allow CRUD operations
7. Dark theme consistent across all pages
8. Responsive on tablet-width screens

---

## Estimated Scope

| Metric | Value |
|--------|-------|
| New files | ~15 (TS/TSX) |
| Modified files | ~7 |
| Estimated LOC | ~2,500 |
| New UI dependencies | 2 (uplot, highlight.js) |
| New routes | 5 (logs, traces, trace detail, metrics, alerts, session detail) |
| New components | 8 (shared components) |
