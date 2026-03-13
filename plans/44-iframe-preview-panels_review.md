# Review: 44-iframe-preview-panels (PR 3 + PR 4)

**Date:** 2026-03-13
**Scope:** PR 3 (Frontend Session View Redesign + Iframe Panels) + PR 4 (Agent Instructions + Templates)
- `ui/src/pages/SessionDetail.tsx` (modified — workspace layout redesign)
- `ui/src/components/IframePanel.tsx` (new — iframe preview component)
- `ui/src/style.css` (modified — workspace/iframe/session-bar CSS)
- `src/agent/create_app_prompt.rs` (modified — +1 prompt line, +2 unit tests)
- `src/git/templates.rs` (modified — +3 unit tests)
- `src/git/templates/CLAUDE.md` (modified — +36 lines Visual Preview section)
- `plans/44-iframe-preview-panels.md` (modified — progress checkboxes)

**Overall:** PASS WITH FINDINGS

## Summary
- Clean implementation: workspace layout, iframe panel, template additions all well-structured
- 0 critical, 0 high, 2 medium findings
- 5 new unit tests added (PR 4), all passing. No backend test gaps — PR 3 is UI-only, PR 4 is template/prompt text
- Touched-line coverage: 100% for Rust changes (all new lines are in unit-tested `const` strings and `#[cfg(test)]` blocks)

## Medium Findings (should fix)

### R1: [MEDIUM] Unused `timeAgo` import after sidebar removal
- **File:** `ui/src/pages/SessionDetail.tsx:4`
- **Domain:** UI / Code cleanliness
- **Description:** `timeAgo` is imported from `../lib/format` but is no longer used. The sidebar that displayed `timeAgo(session.created_at)` was removed in the workspace redesign. The UI build doesn't flag unused TS imports but this is dead code.
- **Suggested fix:** Remove `timeAgo` from the import: `import { duration } from '../lib/format';`

### R2: [MEDIUM] `activeTab` state can become stale when panels list shrinks
- **File:** `ui/src/components/IframePanel.tsx:14`
- **Domain:** UI / Logic
- **Description:** If the panels list shrinks (e.g., iframe service removed while viewing tab 3 of 3), `activeTab` stays at index 2 but only 2 panels remain. The `|| panels[0]` fallback shows correct content, but the tab bar would display no tab as "active" since the active index doesn't match any tab. This is a minor visual glitch, not a crash.
- **Suggested fix:** Clamp `activeTab` when panels change:
  ```tsx
  const clamped = Math.min(activeTab, panels.length - 1);
  const active = panels[clamped];
  ```
  Or reset to 0 when `panels` reference changes via `useEffect`.

## Low Findings (optional)

- [LOW] R3: `ui/src/pages/SessionDetail.tsx:43` — `sseRef` is set but never read. Pre-existing (not introduced in this PR), but worth noting for future cleanup.

## Coverage — Touched Lines

| File | Lines changed | Lines covered | Coverage % | Uncovered lines |
|---|---|---|---|---|
| `src/agent/create_app_prompt.rs` | 3 (prompt + tests) | 3 | 100% | — |
| `src/git/templates.rs` | 24 (tests) | 24 | 100% | — |
| `src/git/templates/CLAUDE.md` | 36 (template text) | N/A (static string) | 100% | — |
| `ui/src/pages/SessionDetail.tsx` | ~80 | N/A (UI) | N/A | — |
| `ui/src/components/IframePanel.tsx` | 53 | N/A (UI) | N/A | — |
| `ui/src/style.css` | ~40 | N/A (CSS) | N/A | — |

### Notes
- All Rust changes are either `const` string literals (embedded at compile time, tested via unit tests that check content) or `#[cfg(test)]` blocks. 100% covered.
- UI/CSS changes have no automated test coverage (standard for this project — Preact components are validated via build + manual testing).

## Checklist Results

| Category | Status | Notes |
|---|---|---|
| Error handling | PASS | No new error paths |
| Auth & permissions | PASS | No new handlers |
| Input validation | PASS | No new user input |
| Audit logging | PASS | No new mutations |
| Tracing instrumentation | PASS | No new async functions |
| Clippy compliance | PASS | `cargo clippy` clean |
| Test patterns | PASS | 5 new unit tests follow existing patterns |
| Migration safety | PASS | No migrations |
| Touched-line coverage | PASS | 100% for Rust code |
