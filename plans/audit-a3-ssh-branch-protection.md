# Plan: A3 — SSH Branch Protection Enforcement

## Context

The codebase audit (A3) and security audit (S1, S2) both identify the same critical gap: **SSH push completely bypasses branch protection rules.** The HTTP `receive_pack` handler (`src/git/smart_http.rs:472`) reads the entire pack body, parses ref update commands with `parse_pack_commands()`, and calls `enforce_push_protection()` before piping data to `git receive-pack`. In contrast, the SSH `exec_request` handler (`src/git/ssh_server.rs:222-266`) spawns `git receive-pack` immediately and pipes SSH channel data directly to git stdin — the platform never sees which refs are being updated and cannot enforce protection.

A related finding (S2) notes that the SSH post-push hook passes `pushed_branches: Vec::new()` and `pushed_tags: Vec::new()` in `handle_post_push()` (line 360-361), meaning pipelines do not trigger for non-default branches, MR `head_sha` is not updated, and stale reviews are not dismissed after SSH pushes.

Both issues share a root cause: **the SSH path does not intercept the git pack data stream.** Any fix for branch protection enforcement also naturally fixes the empty branch/tag data, since both require parsing ref updates from the SSH data flow.

## Design Decision

Three approaches were evaluated:

**Option A: Parse ref updates from the SSH stdin stream.** Buffer incoming SSH `data()` calls until we have the pkt-line ref command section, parse it, enforce protection, then either forward the buffered data to git or reject the push. The SSH git protocol is stateful (no `--stateless-rpc`), so the first phase of `git receive-pack` is the server writing ref advertisements to stdout, then the client sends ref update commands as pkt-lines, then the PACK data. The client's pkt-line commands use the same format as HTTP (`<4-hex-len><old-sha> <new-sha> <refname>\0<capabilities>`). We can reuse `parse_pack_commands()` from `hooks.rs`.

**Option B: Server-side pre-receive hook.** Install a `pre-receive` shell script in each bare repo's `hooks/` directory. Git invokes this hook before accepting a push, passing `old-sha new-sha ref-name` lines on stdin. The hook calls back to the platform's internal API for protection checks. This works transparently for both HTTP and SSH.

**Option C: Hybrid — parse in SSH data handler, use existing hooks infrastructure.** Buffer SSH data, parse ref commands, call `enforce_push_protection()` inline, then forward to git. No shell scripts, no HTTP callbacks, no hook installation.

### Decision: Option C (inline SSH stream interception)

**Why not Option B (pre-receive hook)?**
- Requires installing hook scripts in every bare repo (existing repos need migration)
- The hook needs HTTP connectivity back to the platform API — adds a network dependency in the push path
- Hook errors produce cryptic git error messages that are hard to debug
- The HTTP path already parses ref commands inline and would not benefit from the hook (it would be redundant enforcement)
- Shell script execution adds attack surface and maintenance burden
- The platform already has `enforce_push_protection()` and `parse_pack_commands()` — no need to reimplement in shell

**Why Option C?**
- Reuses existing `parse_pack_commands()` and `enforce_push_protection()` — no code duplication
- No external dependencies (no HTTP callbacks, no shell scripts)
- Protection check happens before any data reaches git — no partial writes to reject
- Also fixes S2 (empty pushed_branches) because we now know which refs were pushed
- The SSH `data()` callback already receives all client data — we just need to buffer the initial pkt-line section before forwarding

### Protocol flow (SSH stateful `git receive-pack`)

```
Server                          Client
  |-- ref advertisements -------->|   (git stdout → SSH channel)
  |                               |
  |<-- ref update commands -------|   (SSH channel → git stdin) ← INTERCEPT HERE
  |<-- PACK data -----------------|   (SSH channel → git stdin)
  |                               |
  |-- result report -------------->|  (git stdout → SSH channel)
```

In the SSH stateful protocol, the server (git) first writes ref advertisements. The client then sends pkt-line ref commands followed by a `0000` flush, then the PACK binary data. We intercept the client's pkt-line commands by buffering `data()` calls until we see the `0000` flush-pkt that terminates the command section. At that point, we parse with `parse_pack_commands()`, call `enforce_push_protection()`, and either forward all buffered data to git stdin (allowed) or close the channel with an error (rejected).

### Key technical detail

The `data()` callback may deliver the pkt-line commands across multiple calls (TCP fragmentation). We need a small state machine per channel:
1. **Buffering** — accumulate bytes, scan for `0000` flush-pkt
2. **Checked** — protection passed, forward buffered data + all future data directly to git stdin
3. **Rejected** — protection failed, drop all further data, close channel

---

## PR 1: SSH branch protection enforcement + pushed ref extraction

**Scope:** Intercept SSH push data, parse ref commands, enforce branch protection, and pass correct pushed branches/tags to post-receive hooks. Single PR because all changes are tightly coupled.

- [ ] Types & errors defined
- [ ] Migration applied (N/A)
- [ ] Tests written (red phase)
- [ ] Implementation complete (green phase)
- [ ] Integration/E2E tests passing
- [ ] Quality gate passed

### Code Changes

| File | Change |
|---|---|
| `src/git/smart_http.rs` | Make `enforce_push_protection()` `pub(super)` (currently private) so `ssh_server.rs` can call it. |
| `src/git/ssh_server.rs` | Major changes — see detailed breakdown below. |

#### `src/git/smart_http.rs` — minimal change

Change `enforce_push_protection` visibility from `async fn` to `pub(super) async fn`. No logic changes.

#### `src/git/ssh_server.rs` — detailed breakdown

**1. Add `SshPushState` enum and `SshPushContext` struct**

```rust
/// State machine for intercepting SSH push data.
enum SshPushState {
    /// Buffering pkt-line ref commands; not yet forwarded to git.
    Buffering(Vec<u8>),
    /// Protection check passed; forwarding data directly to git stdin.
    Forwarding,
    /// Protection check failed or error; dropping all further data.
    Rejected,
}

/// Context for an in-progress SSH push (receive-pack) operation.
struct SshPushContext {
    state: SshPushState,
    project: super::smart_http::ResolvedProject,
    git_user: GitUser,
    ref_updates: Vec<super::hooks::RefUpdate>,
}
```

**2. Add `push_contexts` field to `SshSessionHandler`**

```rust
struct SshSessionHandler {
    state: AppState,
    git_user: Option<GitUser>,
    git_stdin: HashMap<ChannelId, tokio::process::ChildStdin>,
    push_contexts: HashMap<ChannelId, SshPushContext>,
}
```

**3. Modify `exec_request` for write operations**

For write operations (`!parsed.is_read`), after spawning `git receive-pack`, store a `SshPushContext` with `SshPushState::Buffering(Vec::new())` in `push_contexts`. Do NOT immediately store the stdin in `git_stdin` — it stays in the `SshPushContext` until protection is checked.

Actually, stdin still needs to be stored in `git_stdin` because the git process needs to start (it writes ref advertisements to stdout). But for the push context, we track whether data should be buffered or forwarded. We store stdin in `git_stdin` as before, but `data()` routes through the push state machine instead of writing directly.

Revised approach: store stdin in `git_stdin` AND create a `SshPushContext`. The `data()` handler checks if a push context exists for the channel. If yes, it goes through the state machine. If no (read operations), it writes directly to stdin as before.

**4. Modify `data()` handler**

```rust
async fn data(&mut self, channel_id: ChannelId, data: &[u8], session: &mut Session) -> Result<(), Self::Error> {
    // Check if this channel has a push context (write operation)
    if let Some(ctx) = self.push_contexts.get_mut(&channel_id) {
        match &mut ctx.state {
            SshPushState::Buffering(buf) => {
                buf.extend_from_slice(data);
                // Check if we've received the full pkt-line command section
                if let Some(flush_pos) = find_flush_pkt(buf) {
                    // Parse ref commands from buffered data
                    let ref_updates = super::hooks::parse_pack_commands(buf);
                    ctx.ref_updates = ref_updates.clone();

                    // Run protection check
                    let protection_result = super::smart_http::enforce_push_protection(
                        &self.state, &ctx.project, &ctx.git_user, &ref_updates,
                    ).await;

                    match protection_result {
                        Ok(()) => {
                            // Forward all buffered data to git stdin
                            let buffered = std::mem::take(buf);
                            ctx.state = SshPushState::Forwarding;
                            if let Some(stdin) = self.git_stdin.get_mut(&channel_id) {
                                let _ = stdin.write_all(&buffered).await;
                            }
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "SSH push rejected by branch protection");
                            ctx.state = SshPushState::Rejected;
                            // Close git stdin to abort the process
                            self.git_stdin.remove(&channel_id);
                            // Send error message to client via stderr
                            let msg = match &e {
                                _ => "ERROR: push rejected by branch protection rules\n",
                            };
                            let _ = session.handle().extended_data(channel_id, 1, CryptoVec::from_slice(msg.as_bytes())).await;
                            send_exit_and_close(session.handle(), channel_id, 1);
                            return Ok(());
                        }
                    }
                }
                // If no flush-pkt found yet, keep buffering
                // Safety: cap buffer at 1MB to prevent memory exhaustion from malicious clients
                if buf.len() > 1_048_576 {
                    tracing::warn!("SSH push buffer exceeded 1MB without flush-pkt, rejecting");
                    ctx.state = SshPushState::Rejected;
                    self.git_stdin.remove(&channel_id);
                    send_exit_and_close(session.handle(), channel_id, 1);
                }
            }
            SshPushState::Forwarding => {
                // Forward directly to git stdin
                if let Some(stdin) = self.git_stdin.get_mut(&channel_id)
                    && stdin.write_all(data).await.is_err()
                {
                    self.git_stdin.remove(&channel_id);
                }
            }
            SshPushState::Rejected => {
                // Drop all data silently
            }
        }
        return Ok(());
    }

    // Non-push operation: forward directly (existing behavior)
    if let Some(stdin) = self.git_stdin.get_mut(&channel_id)
        && stdin.write_all(data).await.is_err()
    {
        self.git_stdin.remove(&channel_id);
    }
    Ok(())
}
```

**5. Add `find_flush_pkt()` helper**

```rust
/// Scan a buffer for the `0000` flush-pkt that terminates the ref command section.
///
/// Returns the byte position immediately after the flush-pkt (start of PACK data).
/// Returns `None` if the flush-pkt has not been received yet.
fn find_flush_pkt(buf: &[u8]) -> Option<usize> {
    // The flush-pkt is the 4 bytes "0000" appearing as a standalone pkt-line.
    // We need to walk pkt-lines to find it (can't just search for "0000" as it
    // could appear inside binary data, though in practice ref commands are ASCII).
    let mut pos = 0;
    while pos + 4 <= buf.len() {
        let len_hex = &buf[pos..pos + 4];
        if len_hex == b"0000" {
            return Some(pos + 4);
        }
        let Ok(len_str) = std::str::from_utf8(len_hex) else {
            return None; // Not valid pkt-line
        };
        let pkt_len = match usize::from_str_radix(len_str, 16) {
            Ok(n) if n >= 4 => n,
            _ => return None,
        };
        if pos + pkt_len > buf.len() {
            return None; // Incomplete pkt-line, need more data
        }
        pos += pkt_len;
    }
    None // Need more data
}
```

**6. Modify `handle_post_push` to accept ref_updates**

Change `handle_post_push` to accept an optional `Vec<RefUpdate>` so the SSH path can pass the parsed ref updates:

```rust
async fn handle_post_push(
    state: &AppState,
    user_id: uuid::Uuid,
    user_name: &str,
    project: &super::smart_http::ResolvedProject,
    ref_updates: &[super::hooks::RefUpdate],
) {
    let pushed_branches = super::hooks::extract_pushed_branches(ref_updates);
    let pushed_tags = super::hooks::extract_pushed_tags(ref_updates);

    let params = hooks::PostReceiveParams {
        project_id: project.project_id,
        user_id,
        user_name: user_name.to_string(),
        repo_path: project.repo_disk_path.clone(),
        default_branch: project.default_branch.clone(),
        pushed_branches,
        pushed_tags,
    };
    // ... rest unchanged
}
```

Update the call in the git stdout piping task to pass `ref_updates` from the push context. This requires extracting `ref_updates` from the `SshPushContext` before spawning the background task. Add `ref_updates` to the data captured by the `tokio::spawn` closure.

**7. Clean up push context in `channel_eof`**

```rust
async fn channel_eof(&mut self, channel_id: ChannelId, _session: &mut Session) -> Result<(), Self::Error> {
    self.git_stdin.remove(&channel_id);
    self.push_contexts.remove(&channel_id);
    Ok(())
}
```

**8. Modify `GitUser` — clone support**

`GitUser` needs to be stored in `SshPushContext`. Add `#[derive(Clone)]` to `GitUser` in `smart_http.rs` (it only contains `Uuid`, `String`, `Option<String>`, `Option<Uuid>` — all cloneable).

### Async challenge in `data()`

The `data()` method on `russh::server::Handler` is `async`, which means we can call `enforce_push_protection()` (which is async) directly within it. This is important — we do not need to spawn a task or use channels.

However, `enforce_push_protection()` currently lives in `smart_http.rs` as a private function. It needs to become `pub(super)` so `ssh_server.rs` can call it. Its signature takes `&AppState`, `&ResolvedProject`, `&GitUser`, and `&[RefUpdate]` — all available in the SSH handler.

### Error communication to SSH clients

When a push is rejected, the SSH client needs to see an error message. Git clients display stderr output from the server. In russh, we can send extended data (type 1 = stderr) on the channel before closing it. The `session.handle().extended_data(channel_id, 1, data)` method sends stderr data.

The error message format should match what git itself produces, something like:
```
remote: ERROR: push to protected branch 'main' rejected
remote: Push requires a merge request (branch protection rule)
```

### Test Outline

**Unit tests (in `src/git/ssh_server.rs`):**

| Test | What it asserts |
|---|---|
| `find_flush_pkt_simple` | `b"0000"` returns `Some(4)` |
| `find_flush_pkt_after_one_command` | Single pkt-line + `0000` returns correct position |
| `find_flush_pkt_after_multiple_commands` | Two pkt-lines + `0000` returns correct position |
| `find_flush_pkt_incomplete` | Buffer that ends mid-pkt-line returns `None` |
| `find_flush_pkt_empty` | Empty buffer returns `None` |
| `find_flush_pkt_with_trailing_pack_data` | Commands + `0000` + PACK binary returns position after `0000` |

**Integration tests (new file `tests/git_protection_integration.rs`):**

| Test | What it asserts |
|---|---|
| `http_push_protected_branch_rejected` | Create project + protection rule (require_pr=true on `main`). Simulate HTTP push to `main` by calling `receive_pack` with crafted pkt-line body. Assert 403/Forbidden. |
| `http_push_unprotected_branch_allowed` | Same setup, push to `feature` branch (no protection rule). Assert 200 OK. |
| `http_force_push_blocked` | Protection rule with `block_force_push=true`. Push with non-ancestor old_sha. Assert 403. |
| `http_push_admin_bypass` | Protection with `allow_admin_bypass=true`. Admin user can push. Assert 200. |

Note: Full SSH integration tests (spawning real SSH server + client) are complex. The core protection logic is shared between HTTP and SSH, so testing via HTTP validates the `enforce_push_protection()` function. SSH-specific testing (the buffering state machine and data interception) is covered by the unit tests on `find_flush_pkt` and can be validated via E2E tests if an SSH test harness is added later.

**E2E tests (if SSH server is available in test cluster):**

| Test | What it asserts |
|---|---|
| `ssh_push_protected_branch_rejected` | Real `git push` via SSH to protected branch fails with error message |
| `ssh_push_unprotected_branch_succeeds` | Real `git push` via SSH to unprotected branch succeeds |
| `ssh_push_triggers_correct_pipeline` | After SSH push, the correct branch's pipeline is triggered (not the default branch) |

### Edge cases handled

1. **Buffer size limit** — 1 MB cap on buffered pkt-line data prevents memory exhaustion from malicious clients sending huge pkt-line sections. Normal ref commands are ~100 bytes each; even 10,000 refs is ~1 MB. Beyond that, reject.

2. **Fragmented delivery** — `data()` may be called with partial pkt-lines. The state machine buffers until `find_flush_pkt()` finds the `0000` terminator.

3. **Read operations unaffected** — `push_contexts` is only populated for write operations (`!parsed.is_read`). Read operations (`git-upload-pack`) bypass the state machine entirely.

4. **Admin bypass** — `enforce_push_protection()` already handles `allow_admin_bypass` — SSH gets this for free.

5. **Force push detection** — `enforce_push_protection()` already calls `is_force_push()` — SSH gets this for free.

6. **Tag pushes** — The protection check only looks at `refs/heads/*` (branches), so tag pushes pass through. Pushed tags are correctly extracted by `extract_pushed_tags()` for post-receive hooks.

7. **Git stderr** — When protection rejects a push, we send an error message via SSH channel extended data (stderr) and close with exit code 1. The git client will display this to the user.

8. **Race condition** — Between the protection check and git actually processing the push, a protection rule could be added. This is acceptable (same as HTTP) — protection is checked at push time, not at commit time.

### Verification

1. `just test-unit` — new `find_flush_pkt` tests pass, existing `parse_pack_commands` tests still pass
2. `just test-integration` — HTTP push protection integration tests pass, validating `enforce_push_protection()` works correctly
3. Manual verification:
   - Create a project with `require_pr=true` on `main`
   - `git push` via SSH to `main` — rejected with "push rejected by branch protection rules"
   - `git push` via SSH to `feature/foo` — succeeds
   - `git push --force` via SSH to a branch with `block_force_push=true` — rejected
   - Check that pipeline triggers correctly for non-default branches pushed via SSH

### Risk assessment

- **Low risk:** `enforce_push_protection()` is already battle-tested on the HTTP path. Making it `pub(super)` is a visibility-only change.
- **Medium risk:** The SSH data buffering state machine is new code. The `find_flush_pkt()` function must correctly walk pkt-lines. Incorrect parsing could either block legitimate pushes or let protected pushes through. Mitigation: extensive unit tests on `find_flush_pkt()` + the existing `parse_pack_commands()` tests.
- **Low risk:** The 1 MB buffer limit could theoretically reject a push with an extraordinary number of ref updates. In practice, even pushing 10,000 branches at once would only generate ~1 MB of pkt-line data. If this is a concern, the limit can be increased.

---

## Summary

| Item | Detail |
|---|---|
| **Findings addressed** | A3 (codebase audit), S1 + S2 (security audit) |
| **Approach** | Inline SSH stream interception with buffered pkt-line parsing |
| **Files changed** | 2 (`src/git/ssh_server.rs`, `src/git/smart_http.rs`) |
| **New unit tests** | ~6 (`find_flush_pkt` variations) |
| **New integration tests** | ~4 (HTTP push protection) |
| **New E2E tests** | ~3 (SSH push protection, if SSH test harness available) |
| **Migration** | None |
| **Code reuse** | `parse_pack_commands()`, `enforce_push_protection()`, `extract_pushed_branches/tags()` all reused |
| **Side benefit** | Fixes S2 (empty pushed_branches on SSH) — pipelines now trigger correctly for SSH pushes |
