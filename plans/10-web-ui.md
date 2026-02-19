# 10 — Web UI (Preact SPA)

## Prerequisite
- 01-foundation complete (API serving, rust-embed)
- 02-identity-auth complete (login/session API)
- At least 2-3 backend modules implemented (to have API endpoints to consume)

## Blocks
- Nothing — UI can be built incrementally as APIs become available

## Can Parallelize With
- All backend modules (UI consumes APIs, doesn't block them)

---

## Scope

Extend the existing Preact + esbuild scaffold into a functional dashboard. Dark theme, minimal dependencies, focused on the monitoring/audit use case (primary users are humans observing what agents do). Not a design-heavy effort — functional over pretty.

---

## Existing State

- Preact + preact-router + esbuild configured
- Dark theme CSS in index.html (`#0a0a0a` background, `#e5e5e5` text)
- Entry point: `ui/src/index.tsx` with placeholder App component
- Empty: `ui/src/pages/`, `ui/src/components/`
- Served from binary via `rust-embed`

---

## Deliverables

### 1. Shared Foundation

**`ui/src/lib/api.ts`** — API client
- Base URL auto-detected (same origin)
- `fetch` wrapper with session cookie handling
- JSON request/response helpers
- Error handling: 401 → redirect to login, 403 → show forbidden, 5xx → show error

**`ui/src/lib/ws.ts`** — WebSocket client
- Connect to `/api/.../ws` endpoints
- Auto-reconnect with exponential backoff
- Message parsing (JSON)

**`ui/src/lib/types.ts`** — TypeScript types
- Mirrors Rust API response types: User, Project, Issue, MR, Pipeline, Session, LogEntry, etc.
- Keep in sync manually (or generate from OpenAPI later)

**`ui/src/components/Layout.tsx`** — Shell layout
- Sidebar navigation (collapsible)
- Top bar: user avatar, notifications badge, logout
- Content area
- Dark theme throughout

**`ui/src/components/common/`** — Shared components
- `Table.tsx` — sortable, paginated data table
- `Badge.tsx` — status badge (colored by state)
- `TimeAgo.tsx` — relative timestamp
- `Pagination.tsx` — cursor-based pagination controls
- `CodeBlock.tsx` — syntax-highlighted code display
- `Modal.tsx` — dialog/modal
- `Toast.tsx` — toast notification system

### 2. Auth Pages

**`ui/src/pages/Login.tsx`**
- Username + password form
- POST to `/api/auth/login`
- On success: redirect to dashboard
- Error display for invalid credentials

### 3. Dashboard Page

**`ui/src/pages/Dashboard.tsx`**
- Overview cards: active agent sessions, recent builds, deployment status
- Recent activity feed (last 20 events from audit log)
- Quick links to projects

### 4. Projects Pages

**`ui/src/pages/Projects.tsx`** — project list
- Card or table view
- Search, filter by visibility
- Create project button (opens modal)

**`ui/src/pages/ProjectDetail.tsx`** — project detail (tabbed)
- **Files tab**: tree browser (calls git browser API), click to view file contents
- **Issues tab**: issue list with create button, status filter
- **MRs tab**: merge request list with status badges
- **Builds tab**: pipeline list with status, duration, trigger info
- **Deploy tab**: deployment status per environment, rollback button
- **Sessions tab**: agent session list with status
- **Settings tab**: project config, webhooks, secrets (names only)

**`ui/src/pages/IssueDetail.tsx`** — single issue view
- Title, body (markdown rendered), status, labels, assignee
- Comment thread
- Add comment form

**`ui/src/pages/MRDetail.tsx`** — merge request view
- Title, body, source/target branch
- Diff summary (files changed)
- Review list with verdicts
- Merge button (if open)

### 5. Agent Session Pages

**`ui/src/pages/SessionDetail.tsx`** — live agent session view
- **Primary view**: real-time streaming output (WebSocket)
  - Progress events rendered as a timeline
  - Tool calls highlighted (file edits, bash commands)
  - Thinking/reasoning in collapsible blocks
- **Chat input**: send messages to running agent
- **Sidebar**: session metadata (project, branch, cost, duration)
- **Stop button**: terminate session

### 6. Build Pages

**`ui/src/pages/PipelineDetail.tsx`** — pipeline detail
- Step list with status badges, duration
- Click step → view logs (streamed if running, static if complete)
- Artifacts list with download links
- Cancel button for running pipelines

### 7. Observability Pages

**`ui/src/pages/Logs.tsx`** — log viewer
- Time range picker
- Filter bar: project, session, level, service, full-text search
- Log table with expandable rows (show attributes JSON)
- Live tail toggle (WebSocket streaming)

**`ui/src/pages/Traces.tsx`** — trace list + waterfall
- Trace list: root span name, service, duration, status
- Click trace → waterfall view:
  - Horizontal bars showing span timeline
  - Click span → attributes, events panel
  - Links to related logs

**`ui/src/pages/Metrics.tsx`** — metric charts
- Metric name selector
- Label filter
- Time-series line chart (use a lightweight charting lib or canvas)
- Multiple series overlay

**`ui/src/pages/Alerts.tsx`** — alert management
- Alert rule list with enabled/disabled toggle
- Create/edit alert rule form
- Alert event history (firing/resolved timeline)

### 8. Admin Pages

**`ui/src/pages/admin/Users.tsx`** — user management
- User table: name, email, roles, active status
- Create user form
- Edit user: assign/remove roles

**`ui/src/pages/admin/Roles.tsx`** — role & permission management
- Role list with permission counts
- Click role → permission grid (checkboxes)
- Create custom role

**`ui/src/pages/admin/Delegations.tsx`** — delegation viewer
- Active delegations table: delegator → delegate, permission, project, expires
- Create/revoke delegation

**`ui/src/pages/admin/Tokens.tsx`** — API token management (for current user)
- Token list: name, scopes, last used, created
- Create token form (shows raw token once)
- Revoke button

---

## Routing

```typescript
<Router>
  <Login path="/login" />
  <Dashboard path="/" />
  <Projects path="/projects" />
  <ProjectDetail path="/projects/:id/:tab?" />
  <IssueDetail path="/projects/:id/issues/:number" />
  <MRDetail path="/projects/:id/merge-requests/:number" />
  <PipelineDetail path="/projects/:id/pipelines/:pipelineId" />
  <SessionDetail path="/projects/:id/sessions/:sessionId" />
  <Logs path="/observe/logs" />
  <Traces path="/observe/traces" />
  <TraceDetail path="/observe/traces/:traceId" />
  <Metrics path="/observe/metrics" />
  <Alerts path="/observe/alerts" />
  <Users path="/admin/users" />
  <Roles path="/admin/roles" />
  <Delegations path="/admin/delegations" />
  <Tokens path="/settings/tokens" />
</Router>
```

---

## Implementation Priority

Build pages in the order that corresponding APIs become available:

1. **Login + Layout + Dashboard** — once auth API exists
2. **Projects + Issues + MRs** — once project-mgmt API exists
3. **Sessions** (agent streaming) — once agent API exists
4. **Pipelines** — once build engine API exists
5. **Logs + Traces + Metrics** — once observability API exists
6. **Admin pages** — once RBAC admin API exists
7. **Alerts + Deploy** — once those APIs exist

---

## Design Notes

- **No CSS framework** — hand-written CSS with CSS custom properties for theming
- **No state management library** — Preact signals or simple useState/useContext
- **Chart library**: consider `uplot` (tiny, fast) for metrics charts, or canvas-based custom
- **Markdown**: `marked` for rendering issue/MR bodies
- **Code highlighting**: `highlight.js` (lazy-loaded) for file browser

---

## Testing

- Manual testing against running API (no unit tests for UI initially)
- Verify: login flow, project CRUD, issue creation, MR merge, agent streaming, log search
- Browser: Chrome + Firefox

## Done When

1. Login/logout works with session cookies
2. Dashboard shows overview of platform activity
3. Project management: create, browse files, issues, MRs
4. Agent sessions: create, stream live output, send messages
5. Pipeline: view status, step logs, artifacts
6. Observability: log search, trace waterfall, metric charts
7. Admin: user/role management, delegations

## Estimated LOC
~2,500 TypeScript
