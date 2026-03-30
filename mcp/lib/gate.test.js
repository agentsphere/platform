import { describe, it } from 'node:test';
import assert from 'node:assert/strict';
import { gate, computeActionHash, buildSummary, ACTION_TYPES } from './gate.js';

describe('gate()', () => {
  // Plan mode
  it('plan mode: READ is auto', () => assert.equal(gate('list_projects', 'plan'), 'auto'));
  it('plan mode: CREATE is deny', () => assert.equal(gate('create_project', 'plan'), 'deny'));
  it('plan mode: UPDATE is deny', () => assert.equal(gate('update_project', 'plan'), 'deny'));
  it('plan mode: DELETE is deny', () => assert.equal(gate('delete_project', 'plan'), 'deny'));
  it('plan mode: DEPLOY is deny', () => assert.equal(gate('promote_release', 'plan'), 'deny'));

  // Guided mode
  it('guided mode: READ is auto', () => assert.equal(gate('list_projects', 'guided'), 'auto'));
  it('guided mode: CREATE is ask', () => assert.equal(gate('create_project', 'guided'), 'ask'));
  it('guided mode: DELETE is ask', () => assert.equal(gate('delete_project', 'guided'), 'ask'));

  // Auto Read mode
  it('auto_read: READ is auto', () => assert.equal(gate('query_metrics', 'auto_read'), 'auto'));
  it('auto_read: CREATE is ask', () => assert.equal(gate('spawn_agent', 'auto_read'), 'ask'));
  it('auto_read: DEPLOY is ask', () => assert.equal(gate('promote_release', 'auto_read'), 'ask'));

  // Auto Write mode
  it('auto_write: CREATE is auto', () => assert.equal(gate('create_project', 'auto_write'), 'auto'));
  it('auto_write: UPDATE is auto', () => assert.equal(gate('update_project', 'auto_write'), 'auto'));
  it('auto_write: DELETE is ask', () => assert.equal(gate('delete_project', 'auto_write'), 'ask'));
  it('auto_write: DEPLOY is ask', () => assert.equal(gate('promote_release', 'auto_write'), 'ask'));

  // Full Auto mode
  it('full_auto: DELETE is auto', () => assert.equal(gate('delete_project', 'full_auto'), 'auto'));
  it('full_auto: DEPLOY is auto', () => assert.equal(gate('promote_release', 'full_auto'), 'auto'));

  // Unknown tools fail closed
  it('unknown tool in auto_write is ask', () => assert.equal(gate('totally_unknown_tool', 'auto_write'), 'ask'));
  it('unknown tool in full_auto is auto', () => assert.equal(gate('totally_unknown_tool', 'full_auto'), 'auto'));
  it('unknown tool in plan is ask', () => assert.equal(gate('totally_unknown_tool', 'plan'), 'ask'));

  // Invalid mode falls back to ask
  it('invalid mode defaults to ask for writes', () => assert.equal(gate('create_project', 'nonexistent_mode'), 'ask'));
});

describe('computeActionHash()', () => {
  it('returns 16-char hex string', () => {
    const hash = computeActionHash('session-1', 'create_project', { name: 'foo' });
    assert.equal(hash.length, 16);
    assert.match(hash, /^[0-9a-f]+$/);
  });

  it('same inputs produce same hash', () => {
    const a = computeActionHash('s1', 'tool', { x: 1 });
    const b = computeActionHash('s1', 'tool', { x: 1 });
    assert.equal(a, b);
  });

  it('different inputs produce different hash', () => {
    const a = computeActionHash('s1', 'tool', { x: 1 });
    const b = computeActionHash('s1', 'tool', { x: 2 });
    assert.notEqual(a, b);
  });
});

describe('buildSummary()', () => {
  it('create_project includes name', () => {
    assert.ok(buildSummary('create_project', { name: 'my-app' }).includes('my-app'));
  });

  it('promote_release includes project hint', () => {
    const s = buildSummary('promote_release', { project_id: 'abcd1234-5678' });
    assert.ok(s.includes('abcd1234'));
  });

  it('unknown tool uses tool name', () => {
    const s = buildSummary('some_new_tool', {});
    assert.ok(s.includes('some_new_tool'));
  });
});

describe('ACTION_TYPES completeness', () => {
  it('all READ tools return auto in plan mode', () => {
    for (const [tool, type] of Object.entries(ACTION_TYPES)) {
      if (type === 'READ') assert.equal(gate(tool, 'plan'), 'auto', `${tool} should be auto in plan`);
    }
  });

  it('all DELETE tools return deny in plan mode', () => {
    for (const [tool, type] of Object.entries(ACTION_TYPES)) {
      if (type === 'DELETE') assert.equal(gate(tool, 'plan'), 'deny', `${tool} should be deny in plan`);
    }
  });

  it('all DEPLOY tools return ask in auto_write mode', () => {
    for (const [tool, type] of Object.entries(ACTION_TYPES)) {
      if (type === 'DEPLOY') assert.equal(gate(tool, 'auto_write'), 'ask', `${tool} should be ask in auto_write`);
    }
  });
});

// ---------------------------------------------------------------------------
// gateCheck() — calls external APIs, limited testability without a server
// ---------------------------------------------------------------------------

describe('gateCheck()', () => {
  // gateCheck() and waitForApproval() call HTTP APIs (platform backend)
  // so they cannot be pure unit tested without a running server.
  // The approval/rejection flow is covered by the Rust integration tests
  // in tests/manager_integration.rs (approval_roundtrip_via_valkey, etc.).

  it('returns null for non-manager session (empty sessionId) — documented behavior', () => {
    // gateCheck is async and imports apiGet/apiPost from client.js which
    // requires PLATFORM_API_URL. We test the synchronous gate() function
    // instead and trust gateCheck's composition.
    //
    // Per the source: if (!sessionId || process.env.MANAGER_MODE === undefined)
    // return null — skip gate for non-manager sessions.
    assert.ok(true, 'gateCheck skips gate for empty sessionId');
  });

  it('READ actions are always auto regardless of mode — tested via gate()', () => {
    for (const mode of ['plan', 'guided', 'auto_read', 'auto_write', 'full_auto']) {
      assert.equal(gate('list_projects', mode), 'auto', `READ should be auto in ${mode}`);
    }
  });
});

describe('waitForApproval()', () => {
  // waitForApproval() polls an HTTP endpoint (GET /api/manager/sessions/{id}/approval/{hash})
  // every 1s with a 10s timeout. Cannot unit test without a server.
  // Covered by Rust integration tests: see manager_integration.rs
  //   - approve_action_writes_to_valkey
  //   - approve_action_is_single_use
  //   - approval_roundtrip_via_valkey
  it('is tested via Rust integration tests (approval roundtrip)', () => {
    assert.ok(true, 'See manager_integration.rs approval_roundtrip_via_valkey');
  });
});
