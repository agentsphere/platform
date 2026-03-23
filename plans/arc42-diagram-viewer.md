# Plan: Interactive Architecture Viewer (Standalone Docs UI)

A standalone single-page app for exploring the platform's arc42 architecture. Seamless zoom between C4 layers, flow diagrams, component-to-flow cross-referencing. Not part of the platform UI — lives under `docs/viewer/` and deploys as a static site.

---

## Core Interactions

```
┌─────────────────────────────────────────────────────────────────────┐
│ ┌──────────┐  ┌──────────────────────────────────────────────────┐  │
│ │ LEFT     │  │              MAIN CANVAS                        │  │
│ │ SIDEBAR  │  │                                                 │  │
│ │          │  │   ┌─────────────────────────────────────────┐   │  │
│ │ VIEWS    │  │   │                                         │   │  │
│ │ ○ C1     │  │   │    Current C4 level diagram             │   │  │
│ │ ● C2  ←──┼──┼── │    (click node → zoom into C3)          │   │  │
│ │ ○ C3     │  │   │    (click background → zoom out to C1)  │   │  │
│ │ ○ C4     │  │   │                                         │   │  │
│ │          │  │   │    Double-click node → show flows panel  │   │  │
│ │──────────│  │   │                                         │   │  │
│ │ FLOWS    │  │   └─────────────────────────────────────────┘   │  │
│ │ ▸ CI/CD  │  │                                                 │  │
│ │ ▸ Auth   │  │   ┌──────── BREADCRUMB ────────────────────┐   │  │
│ │ ▸ Deploy │  │   │ System › Platform › Agent Module        │   │  │
│ │ ▸ Agent  │  │   └────────────────────────────────────────┘   │  │
│ │ ▸ Observe│  │                                                 │  │
│ │ ▸ GitOps │  │   [ C1 ]  [ C2 ]  [ C3 ]   ← level switcher   │  │
│ │          │  │                                                 │  │
│ └──────────┘  └──────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────────────┘
```

---

## Tech Stack

| Choice | Rationale |
|---|---|
| **Svelte 5** | Tiny bundle (~5KB runtime), reactive, animations built-in (`svelte/transition`, `svelte/animate`). No virtual DOM overhead for SVG manipulation. |
| **Vite** | Fast dev server, static site output (`vite build` → `docs/viewer/dist/`) |
| **Mermaid.js** | Renders C4/flowchart/sequence/state diagrams to SVG in-browser |
| **D3-zoom** | Only `d3-zoom` + `d3-selection` (~15KB) for pan/zoom on the SVG canvas — not all of D3 |

**Why Svelte over Preact**: This is a docs tool, not the platform UI. Svelte's built-in transitions (`fly`, `scale`, `crossfade`) and small bundle make it ideal for a smooth diagram explorer. Zero overlap with platform concerns.

**Why not pure Mermaid**: Mermaid renders static SVGs. We need click handlers on nodes, zoom transitions between levels, and component-to-flow cross-referencing. Mermaid generates the SVG; we add the interactivity layer on top.

---

## File Structure

```
docs/viewer/
├── package.json
├── vite.config.js
├── index.html
├── src/
│   ├── main.js                    # Entry point, mount Svelte app
│   ├── App.svelte                 # Shell: sidebar + canvas + breadcrumb
│   ├── stores/
│   │   ├── navigation.js          # Current level, selected node, breadcrumb stack
│   │   └── search.js              # Filter nodes/flows
│   ├── components/
│   │   ├── Sidebar.svelte         # Left menu: views + flows
│   │   ├── Canvas.svelte          # Main viewport: Mermaid SVG + d3-zoom
│   │   ├── Breadcrumb.svelte      # System › Platform › Module
│   │   ├── LevelSwitcher.svelte   # C1/C2/C3/C4 toggle bar
│   │   ├── FlowViewer.svelte      # Single flow sequence/flowchart
│   │   ├── FlowPanel.svelte       # Slide-out panel showing involved flows
│   │   ├── NodeTooltip.svelte     # Hover tooltip on diagram nodes
│   │   └── MermaidRenderer.svelte # Wrapper: Mermaid.render() → SVG DOM
│   ├── data/
│   │   ├── architecture.js        # C4 graph model (nodes, edges, layers, children)
│   │   ├── flows.js               # Runtime flow definitions (participants, steps)
│   │   ├── mappings.js            # node-id → [flow-ids] cross-reference
│   │   └── mermaid-templates.js   # Functions that generate Mermaid DSL from data
│   ├── lib/
│   │   ├── zoom.js                # d3-zoom setup + animated transitions
│   │   ├── svg-interactivity.js   # Post-render: attach click/hover to SVG nodes
│   │   └── transitions.js         # Svelte transition configs (zoom-in, zoom-out, crossfade)
│   └── styles/
│       ├── global.css             # Reset, fonts, CSS custom properties
│       ├── sidebar.css
│       └── canvas.css
└── static/
    └── favicon.svg
```

---

## Data Model

### Architecture Graph (`data/architecture.js`)

```js
export const architecture = {
  // Every node at every level
  nodes: {
    // C1 — System Context
    "system:platform":    { level: 1, label: "AgentSphere Platform", type: "system", description: "Unified AI DevOps platform" },
    "person:developer":   { level: 1, label: "Developer", type: "person", description: "Pushes code, creates MRs" },
    "person:operator":    { level: 1, label: "Operator", type: "person", description: "Manages platform, monitors" },
    "ext:claude-api":     { level: 1, label: "Claude API", type: "external", description: "Anthropic LLM" },
    "ext:postgres":       { level: 1, label: "PostgreSQL", type: "database", description: "Primary data store" },
    "ext:valkey":         { level: 1, label: "Valkey", type: "database", description: "Cache, pub/sub" },
    "ext:minio":          { level: 1, label: "MinIO", type: "database", description: "Object storage (S3)" },
    "ext:k8s":            { level: 1, label: "Kubernetes", type: "external", description: "Pod orchestration" },
    "ext:smtp":           { level: 1, label: "SMTP", type: "external", description: "Email delivery" },

    // C2 — Containers (modules inside platform)
    "mod:api":            { level: 2, parent: "system:platform", label: "API Layer", tech: "axum", description: "25 HTTP routers" },
    "mod:auth":           { level: 2, parent: "system:platform", label: "Auth & RBAC", tech: "argon2 + Valkey" },
    "mod:git":            { level: 2, parent: "system:platform", label: "Git Server", tech: "russh + smart HTTP" },
    "mod:pipeline":       { level: 2, parent: "system:platform", label: "Pipeline Engine", tech: "K8s pods" },
    "mod:deployer":       { level: 2, parent: "system:platform", label: "Deployer", tech: "Reconciler" },
    "mod:agent":          { level: 2, parent: "system:platform", label: "Agent Orchestrator", tech: "K8s pods + CLI" },
    "mod:observe":        { level: 2, parent: "system:platform", label: "Observability", tech: "OTLP + Parquet" },
    "mod:registry":       { level: 2, parent: "system:platform", label: "OCI Registry", tech: "v2 API" },
    "mod:secrets":        { level: 2, parent: "system:platform", label: "Secrets Engine", tech: "AES-256-GCM" },
    "mod:notify":         { level: 2, parent: "system:platform", label: "Notifications", tech: "SMTP + webhooks" },
    "mod:store":          { level: 2, parent: "system:platform", label: "Store (AppState)", tech: "Shared state" },

    // C3 — Components (inside Agent module)
    "comp:agent:service":       { level: 3, parent: "mod:agent", label: "Session Service", description: "Lifecycle management" },
    "comp:agent:identity":      { level: 3, parent: "mod:agent", label: "Identity Provider", description: "Ephemeral tokens + delegation" },
    "comp:agent:claude_code":   { level: 3, parent: "mod:agent", label: "Claude Code Provider", description: "K8s pod spawning" },
    "comp:agent:claude_cli":    { level: 3, parent: "mod:agent", label: "CLI Session Manager", description: "NDJSON subprocess pool" },
    "comp:agent:create_app":    { level: 3, parent: "mod:agent", label: "App Creator", description: "Orchestrate scaffolding" },
    "comp:agent:preview":       { level: 3, parent: "mod:agent", label: "Preview Watcher", description: "Monitor preview deploys" },
    "comp:agent:pubsub":        { level: 3, parent: "mod:agent", label: "PubSub Bridge", description: "Valkey messaging" },

    // C3 — inside Deployer
    "comp:deployer:reconciler": { level: 3, parent: "mod:deployer", label: "Reconciler", description: "Desired vs actual state loop" },
    "comp:deployer:applier":    { level: 3, parent: "mod:deployer", label: "Applier", description: "kubectl server-side apply" },
    "comp:deployer:renderer":   { level: 3, parent: "mod:deployer", label: "Renderer", description: "Minijinja template engine" },
    "comp:deployer:ops_repo":   { level: 3, parent: "mod:deployer", label: "Ops Repo Manager", description: "Git ops repo CRUD" },
    "comp:deployer:preview":    { level: 3, parent: "mod:deployer", label: "Preview Envs", description: "Ephemeral branch namespaces" },
    "comp:deployer:gateway":    { level: 3, parent: "mod:deployer", label: "Gateway", description: "HTTPRoute + traffic splitting" },
    "comp:deployer:analysis":   { level: 3, parent: "mod:deployer", label: "Analysis", description: "Canary metric evaluation" },

    // C3 — inside Pipeline
    "comp:pipeline:definition": { level: 3, parent: "mod:pipeline", label: "Definition Parser", description: ".platform.yaml parsing" },
    "comp:pipeline:executor":   { level: 3, parent: "mod:pipeline", label: "Executor", description: "Step dispatch + pod management" },
    "comp:pipeline:trigger":    { level: 3, parent: "mod:pipeline", label: "Trigger", description: "Event matching + pipeline creation" },

    // ... (C3 for other modules, C4 for code-level when needed)
  },

  // Edges between nodes (rendered as arrows)
  edges: [
    // C1 edges
    { from: "person:developer", to: "system:platform", label: "Git push, API", protocol: "HTTPS/SSH" },
    { from: "system:platform", to: "ext:postgres", label: "Queries", protocol: "SQL/TLS" },
    { from: "system:platform", to: "ext:claude-api", label: "Agent sessions", protocol: "HTTPS" },
    // ...

    // C2 edges (visible when zoomed into platform)
    { from: "mod:api", to: "mod:auth", label: "AuthUser extractor" },
    { from: "mod:api", to: "mod:pipeline", label: "Trigger pipeline" },
    { from: "mod:pipeline", to: "mod:deployer", label: "OpsRepoUpdated event" },
    { from: "mod:deployer", to: "mod:secrets", label: "Inject secrets" },
    // ...

    // C3 edges (visible when zoomed into a module)
    { from: "comp:pipeline:trigger", to: "comp:pipeline:executor", label: "notify_one()" },
    { from: "comp:deployer:reconciler", to: "comp:deployer:renderer", label: "render_manifests()" },
    { from: "comp:deployer:renderer", to: "comp:deployer:applier", label: "apply_manifests()" },
    // ...
  ],
};
```

### Flows (`data/flows.js`)

```js
export const flows = {
  "flow:cicd-overview": {
    id: "flow:cicd-overview",
    name: "Full CI/CD Lifecycle",
    category: "CI/CD",
    description: "MR → Build → Merge → Deploy Staging → Promote → Production",
    type: "flowchart",
    involvedNodes: [
      "mod:git", "mod:pipeline", "mod:deployer", "mod:secrets",
      "mod:observe", "mod:registry",
      "comp:pipeline:trigger", "comp:pipeline:executor",
      "comp:deployer:reconciler", "comp:deployer:analysis",
    ],
    mermaid: `flowchart LR
      subgraph Code["Code Repo"]
        push[Git Push] --> hook[Post-Receive Hook]
      end
      ...`,
  },

  "flow:mr-pipeline": {
    id: "flow:mr-pipeline",
    name: "MR Pipeline",
    category: "CI/CD",
    type: "sequence",
    involvedNodes: ["mod:git", "mod:pipeline", "comp:pipeline:trigger", "comp:pipeline:executor", "mod:registry"],
    mermaid: `sequenceDiagram
      Developer->>Git Server: push to feature branch
      Git Server->>EventBus: post-receive hook
      ...`,
  },

  "flow:merge-gitops": {
    id: "flow:merge-gitops",
    name: "Auto-Merge + GitOps Sync",
    category: "CI/CD",
    type: "sequence",
    involvedNodes: ["mod:pipeline", "mod:git", "mod:deployer", "comp:pipeline:executor", "comp:deployer:ops_repo"],
    mermaid: `sequenceDiagram ...`,
  },

  "flow:deploy-canary": {
    id: "flow:deploy-canary",
    name: "Deploy + Canary Progression",
    category: "CI/CD",
    type: "sequence",
    involvedNodes: ["mod:deployer", "mod:secrets", "comp:deployer:reconciler", "comp:deployer:analysis", "comp:deployer:applier", "comp:deployer:renderer", "comp:deployer:gateway"],
    mermaid: `sequenceDiagram ...`,
  },

  "flow:auth": {
    id: "flow:auth",
    name: "Authentication Flow",
    category: "Auth",
    type: "sequence",
    involvedNodes: ["mod:auth", "mod:api", "ext:postgres", "ext:valkey"],
    mermaid: `sequenceDiagram ...`,
  },

  "flow:agent-session": {
    id: "flow:agent-session",
    name: "Agent Session Lifecycle",
    category: "Agent",
    type: "sequence",
    involvedNodes: ["mod:agent", "mod:auth", "ext:k8s", "comp:agent:service", "comp:agent:identity", "comp:agent:claude_cli"],
    mermaid: `sequenceDiagram ...`,
  },

  "flow:observe-pipeline": {
    id: "flow:observe-pipeline",
    name: "Observability Pipeline",
    category: "Observability",
    type: "flowchart",
    involvedNodes: ["mod:observe", "ext:minio"],
    mermaid: `flowchart LR ...`,
  },

  "flow:state-pipeline": {
    id: "flow:state-pipeline",
    name: "Pipeline State Machine",
    category: "State Machines",
    type: "state",
    involvedNodes: ["mod:pipeline", "comp:pipeline:executor"],
    mermaid: `stateDiagram-v2 ...`,
  },

  "flow:state-deployment": {
    id: "flow:state-deployment",
    name: "Deployment State Machine",
    category: "State Machines",
    type: "state",
    involvedNodes: ["mod:deployer", "comp:deployer:reconciler", "comp:deployer:analysis"],
    mermaid: `stateDiagram-v2 ...`,
  },
};
```

### Cross-Reference Mapping (`data/mappings.js`)

```js
// Auto-generated from flows[].involvedNodes
// Inverted index: nodeId → [flowIds]
export function buildNodeFlowMap(flows) {
  const map = {};
  for (const [flowId, flow] of Object.entries(flows)) {
    for (const nodeId of flow.involvedNodes) {
      (map[nodeId] ??= []).push(flowId);
    }
  }
  return map;
}

// Result example:
// "mod:pipeline" → ["flow:cicd-overview", "flow:mr-pipeline", "flow:merge-gitops"]
// "mod:auth"     → ["flow:auth"]
// "comp:deployer:reconciler" → ["flow:cicd-overview", "flow:deploy-canary"]
```

---

## Navigation State Machine

```
              ┌──────────┐
              │  C1 View │  (System Context)
              └────┬─────┘
                   │ click "Platform" node
                   ▼
              ┌──────────┐
              │  C2 View │  (Containers / Modules)
              └────┬─────┘
                   │ click "Agent" or "Deployer" node
                   ▼
              ┌──────────┐
              │  C3 View │  (Components inside module)
              └────┬─────┘
                   │ click component (optional)
                   ▼
              ┌──────────┐
              │  C4 View │  (Code-level — if data exists)
              └──────────┘

Navigation actions:
  - Click node with children     → zoom IN  (push to breadcrumb)
  - Click breadcrumb ancestor    → zoom OUT (pop breadcrumb)
  - Click level switcher button  → jump to level (rebuild breadcrumb)
  - Click sidebar C1/C2/C3/C4   → same as level switcher
  - Double-click / right-click   → open flow panel for that node
  - Click sidebar flow item      → switch to flow view
  - ESC / back button            → zoom out one level
```

### Navigation Store (`stores/navigation.js`)

```js
import { writable, derived } from 'svelte/store';

export const currentLevel = writable(1);          // 1-4
export const selectedNode = writable(null);        // node being "inside"
export const breadcrumb = writable([]);            // [{id, label, level}]
export const viewMode = writable('architecture');  // 'architecture' | 'flow'
export const activeFlowId = writable(null);        // which flow is displayed
export const flowPanelOpen = writable(false);      // slide-out panel visible
export const flowPanelNodeId = writable(null);     // which node's flows to show

// Derived: which nodes to render at current level
export const visibleNodes = derived(
  [currentLevel, selectedNode],
  ([$level, $selected]) => {
    // Level 1: show all level-1 nodes
    // Level 2: show children of selected level-1 node + external connections
    // Level 3: show children of selected level-2 node
    // ...
  }
);

export function zoomIn(nodeId) { /* push breadcrumb, set level, set selectedNode */ }
export function zoomOut()      { /* pop breadcrumb, restore parent level */ }
export function jumpToLevel(n) { /* rebuild breadcrumb to level n */ }
export function showFlow(id)   { /* viewMode='flow', activeFlowId=id */ }
export function showNodeFlows(nodeId) { /* flowPanelOpen=true, flowPanelNodeId=nodeId */ }
```

---

## Key Components

### Canvas.svelte — Main Viewport

```
┌────────────────────────────────────────────────────┐
│  ┌─ Breadcrumb ──────────────────────────────────┐ │
│  │ System › Platform › Agent Module              │ │
│  └───────────────────────────────────────────────┘ │
│                                                    │
│  ┌─ SVG Container (d3-zoom) ─────────────────────┐ │
│  │                                                │ │
│  │   Mermaid-rendered SVG                         │ │
│  │   + click handlers on nodes                    │ │
│  │   + hover tooltips                             │ │
│  │   + highlighted nodes (when flow panel open)   │ │
│  │                                                │ │
│  └────────────────────────────────────────────────┘ │
│                                                    │
│  ┌─ Level Switcher ──────────────────────────────┐ │
│  │  [ C1 ]  [ C2 ]  [•C3•]  [ C4 ]              │ │
│  └───────────────────────────────────────────────┘ │
└────────────────────────────────────────────────────┘
```

**Zoom transition behavior**:
1. User clicks a node (e.g., "Pipeline Engine" at C2)
2. Canvas zooms INTO that node (d3-zoom animates to center on it, scale up)
3. While zooming, the current SVG fades out (opacity transition, 300ms)
4. Mermaid re-renders the next level's diagram (children of "Pipeline Engine")
5. New SVG fades in at the zoomed-in position, then d3-zoom eases back to fit-to-viewport
6. Total transition: ~500ms

**Zoom out**: reverse — zoom out from center, crossfade to parent level, fit to viewport.

### Sidebar.svelte — Left Menu

```
┌──────────────────────┐
│  Architecture        │  ← section header
│                      │
│  Views               │
│  ┌─ C1 Context     │  ← click → jumpToLevel(1)
│  ├─ C2 Containers  │
│  │  ├─ API         │  ← click → jumpToLevel(2) + select mod:api
│  │  ├─ Auth        │
│  │  ├─ Git         │
│  │  ├─ Pipeline ▸  │  ← ▸ indicates children (C3 available)
│  │  ├─ Deployer ▸  │
│  │  ├─ Agent ▸     │
│  │  ├─ Observe     │
│  │  ├─ Registry    │
│  │  ├─ Secrets     │
│  │  └─ Notify      │
│  └─ C3 Components  │  ← only shown when a C2 node is selected
│     ├─ Reconciler  │
│     ├─ Applier     │
│     └─ ...         │
│                      │
│  ─────────────────── │
│                      │
│  ▸ Flows             │  ← section header (collapsible)
│  ┌─ CI/CD           │  ← category
│  │  ├─ Full Lifecycle│
│  │  ├─ MR Pipeline  │
│  │  ├─ Merge+GitOps │
│  │  └─ Deploy+Canary│
│  ├─ Auth            │
│  │  └─ Login Flow   │
│  ├─ Agent           │
│  │  └─ Session Life.│
│  ├─ Observability   │
│  │  └─ OTLP Pipeline│
│  └─ State Machines  │
│     ├─ Pipeline     │
│     └─ Deployment   │
│                      │
│  ─────────────────── │
│  Search...           │
└──────────────────────┘
```

**Sidebar behavior**:
- Architecture tree reflects current breadcrumb — selecting a node navigates the canvas
- Active node highlighted with accent color
- Nodes with children show `▸` chevron
- Flow items switch `viewMode` to `'flow'` and render the single flow diagram
- Search filters both architecture nodes and flow names

### FlowPanel.svelte — Slide-Out Panel (Component → Flows)

Triggered by double-clicking a node in the C-view.

```
┌──────────────────────────────────────────┬─────────────────────┐
│           MAIN CANVAS                    │   FLOW PANEL        │
│                                          │                     │
│   ┌──────────────────────────────┐       │  Flows involving:   │
│   │                              │       │  "Pipeline Engine"  │
│   │   C2 diagram with            │       │                     │
│   │   "Pipeline" node            │  ←──  │  ┌───────────────┐  │
│   │   highlighted in accent      │       │  │ Full CI/CD    │  │
│   │                              │       │  │ Lifecycle     │  │
│   │   Other nodes dimmed         │       │  │ ───────────── │  │
│   │   (30% opacity)              │       │  │ [View →]      │  │
│   │                              │       │  └───────────────┘  │
│   └──────────────────────────────┘       │  ┌───────────────┐  │
│                                          │  │ MR Pipeline   │  │
│                                          │  │ ───────────── │  │
│                                          │  │ [View →]      │  │
│                                          │  └───────────────┘  │
│                                          │  ┌───────────────┐  │
│                                          │  │ Merge + GitOps│  │
│                                          │  │ [View →]      │  │
│                                          │  └───────────────┘  │
│                                          │         [x Close]   │
└──────────────────────────────────────────┴─────────────────────┘
```

**Behavior**:
- Slides in from right (300ms CSS transition)
- Main canvas shrinks to accommodate (flex layout, animated)
- Selected node highlighted in the diagram, others dimmed
- Each flow card shows name, category, brief description
- "View →" switches to full flow view
- Panel closes on ESC, `x` button, or clicking canvas background

### FlowViewer.svelte — Single Flow View

```
┌────────────────────────────────────────────────────┐
│  ← Back to Architecture    │  MR Pipeline Flow     │
│                                                    │
│  ┌─────────────────────────────────────────────┐   │
│  │                                             │   │
│  │   Mermaid sequenceDiagram / flowchart       │   │
│  │   (full viewport, d3-zoom enabled)          │   │
│  │                                             │   │
│  │   Participants that map to architecture     │   │
│  │   nodes are clickable → navigate to that    │   │
│  │   node in the C-view                        │   │
│  │                                             │   │
│  └─────────────────────────────────────────────┘   │
│                                                    │
│  Involved components:                              │
│  [Git Server] [Pipeline Trigger] [Executor] [K8s]  │
│  ^ chip badges, clickable → navigate to C-view     │
└────────────────────────────────────────────────────┘
```

---

## Mermaid Integration Layer

### MermaidRenderer.svelte

```
Props:
  - definition: string     (Mermaid DSL text)
  - nodeClickHandler: fn   (called with nodeId when SVG node clicked)
  - highlightNodes: string[] (node IDs to highlight)
  - dimOthers: boolean     (dim non-highlighted nodes)

Lifecycle:
  1. On mount / definition change:
     a. Call mermaid.render(id, definition) → SVG string
     b. Insert SVG into container div (innerHTML)
     c. Call svg-interactivity.js to attach click/hover handlers
     d. Apply highlight/dim classes to matching SVG elements
  2. On highlightNodes change:
     a. Update CSS classes on existing SVG (no re-render)
```

### svg-interactivity.js

```js
// After Mermaid renders, we post-process the SVG DOM:

export function attachHandlers(svgElement, { onClick, onDoubleClick, onHover }) {
  // Mermaid C4 diagrams: nodes are <g> elements with class "person", "system", etc.
  // Mermaid flowcharts: nodes are <g class="node">
  // Each has a data-id or aria-label we can map to our architecture node IDs

  const nodes = svgElement.querySelectorAll('.node, .person, .system, .container');

  for (const node of nodes) {
    const label = extractLabel(node);       // text content or data attribute
    const archNodeId = labelToNodeId(label); // map "Pipeline Engine" → "mod:pipeline"

    if (!archNodeId) continue;

    node.style.cursor = 'pointer';
    node.addEventListener('click', () => onClick(archNodeId));
    node.addEventListener('dblclick', () => onDoubleClick(archNodeId));
    node.addEventListener('mouseenter', () => onHover(archNodeId, true));
    node.addEventListener('mouseleave', () => onHover(archNodeId, false));
  }
}

export function highlightNodes(svgElement, nodeIds, dim = true) {
  const allNodes = svgElement.querySelectorAll('.node, .person, .system, .container');
  for (const node of allNodes) {
    const id = labelToNodeId(extractLabel(node));
    if (nodeIds.includes(id)) {
      node.classList.add('highlighted');
      node.classList.remove('dimmed');
    } else if (dim) {
      node.classList.add('dimmed');
      node.classList.remove('highlighted');
    }
  }
}
```

### mermaid-templates.js — Generate Mermaid DSL from Data

```js
// Generate Mermaid DSL from architecture data rather than hand-writing:

export function generateC1(nodes, edges) {
  const c1Nodes = Object.entries(nodes).filter(([_, n]) => n.level === 1);
  let dsl = 'C4Context\n    title System Context\n\n';
  for (const [id, node] of c1Nodes) {
    if (node.type === 'person')   dsl += `    Person(${sanitize(id)}, "${node.label}", "${node.description}")\n`;
    if (node.type === 'system')   dsl += `    System(${sanitize(id)}, "${node.label}", "${node.description}")\n`;
    if (node.type === 'external') dsl += `    System_Ext(${sanitize(id)}, "${node.label}", "${node.description}")\n`;
    if (node.type === 'database') dsl += `    SystemDb(${sanitize(id)}, "${node.label}", "${node.description}")\n`;
  }
  // ... add Rel() for edges
  return dsl;
}

export function generateC2(nodes, edges, parentId) {
  // Filter nodes where parent === parentId
  // Generate C4Container diagram
}

export function generateC3(nodes, edges, parentId) {
  // Filter nodes where parent === parentId
  // Generate C4Component diagram
}
```

---

## Zoom Transition Implementation

### zoom.js — D3-zoom + Animated Transitions

```js
import { zoom, zoomIdentity } from 'd3-zoom';
import { select } from 'd3-selection';

export function setupZoom(svgContainer) {
  const z = zoom()
    .scaleExtent([0.3, 4])
    .on('zoom', (event) => {
      svgContainer.querySelector('svg > g')
        ?.setAttribute('transform', event.transform.toString());
    });

  select(svgContainer).call(z);

  return {
    // Animate zoom to fit entire SVG in viewport
    fitToView(duration = 500) { /* ... */ },

    // Animate zoom INTO a specific node (before level transition)
    zoomToNode(nodeElement, duration = 300) {
      const bbox = nodeElement.getBoundingClientRect();
      const containerRect = svgContainer.getBoundingClientRect();
      const scale = Math.min(
        containerRect.width / bbox.width,
        containerRect.height / bbox.height
      ) * 0.5;
      const x = containerRect.width / 2 - bbox.x * scale;
      const y = containerRect.height / 2 - bbox.y * scale;
      select(svgContainer)
        .transition().duration(duration)
        .call(z.transform, zoomIdentity.translate(x, y).scale(scale));
    },

    // Reset zoom (used after level change, new SVG rendered)
    reset() {
      select(svgContainer)
        .transition().duration(400)
        .call(z.transform, zoomIdentity);
    },
  };
}
```

### Transition Sequence (in Canvas.svelte)

```
User clicks node "mod:pipeline" at C2:

Frame 0ms:    zoomToNode(pipelineElement, 300ms)   ← d3 animates zoom into node
Frame 200ms:  Start fade-out current SVG           ← opacity 1→0 (200ms)
Frame 300ms:  Swap Mermaid definition to C3         ← generateC3(nodes, edges, "mod:pipeline")
Frame 300ms:  Mermaid.render() → new SVG            ← new SVG inserted (opacity 0)
Frame 300ms:  Start fade-in new SVG                 ← opacity 0→1 (200ms)
Frame 400ms:  fitToView(400ms)                      ← d3 eases to fit new diagram
Frame 800ms:  Transition complete                   ← user sees C3 Pipeline components
```

In Svelte, coordinated via `{#key currentDiagram}` blocks with `in:fade` / `out:fade` transitions.

---

## Flow-to-Architecture Cross-Linking

### Architecture → Flows (double-click node in C-view)

1. User double-clicks "Deployer" at C2
2. `showNodeFlows("mod:deployer")` called
3. Look up `nodeFlowMap["mod:deployer"]` → `["flow:cicd-overview", "flow:deploy-canary"]`
4. FlowPanel slides in showing those 2 flows
5. "Deployer" node highlighted, others dimmed

### Flows → Architecture (click participant in flow view)

1. User is viewing "MR Pipeline" flow (sequence diagram)
2. Clicks the "Executor" participant
3. Maps participant label → `"comp:pipeline:executor"` (node at C3)
4. Navigates to architecture view: `jumpToLevel(3)` with parent `"mod:pipeline"` selected
5. `"comp:pipeline:executor"` highlighted in the C3 diagram

### Sidebar badge counts

Each architecture node in the sidebar shows a small badge with the number of flows it's involved in. Example: `Pipeline ▸ (3)`.

---

## Build & Deploy

```json
// package.json scripts
{
  "dev": "vite",
  "build": "vite build --outDir dist",
  "preview": "vite preview"
}
```

```js
// vite.config.js
import { svelte } from '@sveltejs/vite-plugin-svelte';
export default {
  base: '/architecture/',    // GitHub Pages subpath
  plugins: [svelte()],
  build: { outDir: 'dist' },
};
```

**Justfile integration**:
```makefile
docs-viewer:          # build architecture viewer
    cd docs/viewer && npm ci && npm run build

docs-serve:           # dev server for architecture viewer
    cd docs/viewer && npm run dev
```

**Deploy**: `docs/viewer/dist/` → GitHub Pages (or any static host). Fully client-side — no server needed.

---

## Implementation Phases

| Phase | Deliverable | Files | Effort |
|---|---|---|---|
| **P1: Scaffold** | Vite + Svelte project, basic shell (sidebar + empty canvas) | `package.json`, `vite.config.js`, `App.svelte`, `Sidebar.svelte`, `Canvas.svelte`, styles | 1 session |
| **P2: Data layer** | Architecture graph + flow definitions + mermaid templates | `data/architecture.js`, `data/flows.js`, `data/mappings.js`, `data/mermaid-templates.js` | 1-2 sessions |
| **P3: Static rendering** | Mermaid renders diagrams, level switcher works, sidebar navigates | `MermaidRenderer.svelte`, `LevelSwitcher.svelte`, `Breadcrumb.svelte` | 1 session |
| **P4: Zoom transitions** | D3-zoom, animated zoom-in/out between levels, crossfade | `lib/zoom.js`, `lib/transitions.js`, Canvas updates | 1 session |
| **P5: Interactivity** | Click/hover on SVG nodes, node-to-flow mapping, flow panel | `lib/svg-interactivity.js`, `FlowPanel.svelte`, `NodeTooltip.svelte` | 1 session |
| **P6: Flow viewer** | Full flow view with participant-to-architecture linking | `FlowViewer.svelte`, flow-to-arch navigation | 1 session |
| **P7: Polish** | Search, keyboard nav (ESC/arrows), responsive, dark mode | `stores/search.js`, CSS, accessibility | 1 session |
| **Total** | | | **7-8 sessions** |

---

## Future: Code-to-Data Automation

Once the viewer works with hand-authored data, a `just arc42-extract` task can auto-generate `data/architecture.js` from:

| Source | Extract |
|---|---|
| `src/*/mod.rs` | Module names, `pub mod` children → C2/C3 nodes |
| `src/api/mod.rs` | `.route()` calls → API node edges |
| `src/store/mod.rs` | `AppState` fields → external system nodes |
| `src/*/error.rs` | Error types → module responsibility hints |
| `can_transition_to()` | State machine flow diagrams |
| `migrations/*.up.sql` | ER diagram data |
| `deploy/` YAML | Deployment topology nodes |

This closes the "Code → Docs → Diagrams" loop — code changes automatically update the viewer's data.
