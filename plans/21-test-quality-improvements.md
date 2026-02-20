# Plan 21 — Test Quality Improvements

## Overview

Improve the quality and coverage of the existing 264 unit tests. The current suite has weak assertions (`is_err()` instead of specific error matching), tautological tests that pass unconditionally, missing boundary/edge-case tests, and unused dev-dependencies (`proptest`, `rstest`, `insta`). This plan fixes these issues and adds ~65 targeted unit tests for gaps identified in Plan 14.

**This corresponds to Test Quality Phases B-D from Plan 14.**

---

## Motivation

- **Weak assertions hide bugs**: 30+ tests use `.is_err()` without checking _which_ error — a test "passes" even if the code throws the wrong error for the wrong reason
- **Tautological tests are worse than no tests**: `cache_key_deterministic` calls the same function twice and asserts equality — it can never fail. `internal_hides_details` has a short-circuit assertion that never evaluates the real check.
- **Missing boundary tests**: Validation functions have zero tests at exact boundary values (min/max length, edge characters)
- **Unused dev-deps**: `proptest`, `rstest`, and `insta` are in `Cargo.toml` but never used
- **Security-adjacent code untested**: Error conversion (leaking internal details), IPv6 SSRF gaps, HMAC signing, timing-safe auth

---

## Prerequisites

| Requirement | Status |
|---|---|
| `rstest = "0.25"` in dev-deps | Already in Cargo.toml |
| `proptest = "1"` in dev-deps | Already in Cargo.toml |
| `insta = "1"` in dev-deps | Already in Cargo.toml |
| IPv6 SSRF fix from Plan 14A | Should be applied first |

---

## Phase B: Boundary & Edge-Case Unit Tests (~60 tests)

### B1: `src/validation.rs` — Boundary Tests (~20 tests)

| # | Test | Input | Expected |
|---|------|-------|----------|
| 1 | `name_empty_rejected` | `""` | Err (min 1) |
| 2 | `name_single_char_ok` | `"a"` | Ok |
| 3 | `name_at_max_length` | `"a".repeat(255)` | Ok |
| 4 | `name_over_max_length` | `"a".repeat(256)` | Err |
| 5 | `name_with_hyphen_underscore_dot` | `"my-app_v1.0"` | Ok |
| 6 | `name_leading_dot_rejected` | `".hidden"` | Err (if N3 fix applied) |
| 7 | `name_with_spaces_rejected` | `"has space"` | Err |
| 8 | `name_unicode_rejected` | `"café"` | Err (is_alphanumeric is Unicode-aware but we want ASCII only) |
| 9 | `email_minimum_valid` | `"a@b"` | Ok (length 3) |
| 10 | `email_no_at_rejected` | `"nope"` | Err |
| 11 | `email_double_at_rejected` | `"a@@b"` | Err (if N2 fix applied) |
| 12 | `email_at_max_length` | 254-char valid email | Ok |
| 13 | `email_over_max_length` | 255-char email | Err |
| 14 | `branch_name_with_double_dot` | `"main..evil"` | Err |
| 15 | `branch_name_with_null_byte` | `"main\0evil"` | Err |
| 16 | `branch_name_normal` | `"feature/add-login"` | Ok |
| 17 | `labels_empty_vec_ok` | `vec![]` | Ok |
| 18 | `labels_at_max_count` | 50 labels | Ok |
| 19 | `labels_over_max_count` | 51 labels | Err |
| 20 | `labels_empty_string_rejected` | `vec![""]` | Err (min 1 char) |
| 21 | `labels_at_max_char_length` | `vec!["a".repeat(100)]` | Ok |
| 22 | `labels_over_max_char_length` | `vec!["a".repeat(101)]` | Err |
| 23 | `url_http_ok` | `"http://example.com"` | Ok |
| 24 | `url_https_ok` | `"https://example.com"` | Ok |
| 25 | `url_ftp_rejected` | `"ftp://example.com"` | Err |
| 26 | `url_over_max_length` | 2049-char URL | Err |
| 27 | `lfs_oid_valid_64_hex` | `"a".repeat(64)` | Ok |
| 28 | `lfs_oid_63_chars_rejected` | `"a".repeat(63)` | Err |
| 29 | `lfs_oid_65_chars_rejected` | `"a".repeat(65)` | Err |
| 30 | `lfs_oid_non_hex_rejected` | `"g".repeat(64)` | Err |

#### Assertion Pattern (strengthened)

```rust
// BEFORE (weak)
assert!(check_name("").is_err());

// AFTER (strong)
let err = check_name("").unwrap_err();
assert!(
    matches!(err, ApiError::BadRequest(msg) if msg.contains("name")),
    "empty name should produce BadRequest with field name, got: {err:?}"
);
```

---

### B2: `src/error.rs` — Error Conversion Tests (~5 tests)

| # | Test | Scenario | Expected |
|---|------|----------|----------|
| 1 | `sqlx_row_not_found_maps_to_404` | `sqlx::Error::RowNotFound` | `ApiError::NotFound` |
| 2 | `sqlx_unique_violation_maps_to_409` | `sqlx::Error::Database` with code 23505 | `ApiError::Conflict` |
| 3 | `sqlx_generic_error_hides_details` | `sqlx::Error::Protocol("secret")` | `ApiError::Internal` — response body must NOT contain "secret" |
| 4 | `api_error_bad_request_status` | `ApiError::BadRequest("msg")` | 400 status code |
| 5 | `api_error_unauthorized_status` | `ApiError::Unauthorized` | 401 status code |

**Critical fix (C1 from Plan 13):**

```rust
// BEFORE (tautological — always true):
assert!(!body.iter().any(|_| false) || !json.to_string().contains("secret"));

// AFTER:
assert!(
    !json.to_string().contains("secret"),
    "internal error response must not leak error details"
);
```

---

### B3: `src/auth/middleware.rs` — Edge-Case Tests (~5 tests)

| # | Test | Input | Expected |
|---|------|-------|----------|
| 1 | `bearer_token_double_space` | `"Bearer  abc"` (double space) | Token extraction fails or handles gracefully |
| 2 | `bearer_token_no_space` | `"Bearerabc"` | Extraction fails |
| 3 | `bearer_lowercase` | `"bearer abc"` | Case-insensitive extraction (or reject) |
| 4 | `empty_auth_header` | `""` | Returns None |
| 5 | `ipv6_x_forwarded_for` | `"::1"` | Parsed correctly |

---

### B4: `src/secrets/engine.rs` — Crypto Boundary Tests (~5 tests)

| # | Test | Input | Expected |
|---|------|-------|----------|
| 1 | `encrypt_empty_plaintext_roundtrip` | `""` | Decrypt returns `""` |
| 2 | `encrypt_large_plaintext` | 100KB string | Roundtrip succeeds |
| 3 | `master_key_63_hex_rejected` | 63-char hex | Parse error |
| 4 | `master_key_65_hex_rejected` | 65-char hex | Parse error |
| 5 | `master_key_whitespace_handling` | `"  abcdef...  "` | Trimmed before parsing (or rejected) |

---

### B5: `src/pipeline/definition.rs` — Pattern Matching Tests (~8 tests)

| # | Test | Trigger Pattern | Git Ref / Event | Expected |
|---|------|----------------|-----------------|----------|
| 1 | `wildcard_branch_matches_all` | `branches: ["*"]` | `main` | Match |
| 2 | `prefix_wildcard` | `branches: ["release/*"]` | `release/v1.0` | Match |
| 3 | `prefix_wildcard_no_match` | `branches: ["release/*"]` | `feature/foo` | No match |
| 4 | `suffix_wildcard` | `branches: ["*-release"]` | `v2-release` | Match |
| 5 | `empty_branches_matches_none` | `branches: []` | `main` | No match |
| 6 | `empty_actions_defaults_match` | `actions: []` | `push` | Match (empty = all) |
| 7 | `multiple_branches_any_match` | `branches: ["main", "develop"]` | `develop` | Match |
| 8 | `complex_definition_parsing` | Full YAML with steps | — | All steps parsed correctly |

---

### B6: `src/config.rs` — Configuration Edge Cases (~5 tests)

| # | Test | Scenario | Expected |
|---|------|----------|----------|
| 1 | `empty_cors_origins_treated_as_none` | `""` | No CORS origins (deny all) |
| 2 | `cors_origins_whitespace_trimmed` | `" a.com , b.com "` | `["a.com", "b.com"]` |
| 3 | `bool_config_true_variants` | `"TRUE"`, `"1"`, `"yes"` | Should parse as true |
| 4 | `pipeline_namespace_default` | Unset env | `"default"` |
| 5 | `dev_mode_enables_defaults` | `PLATFORM_DEV=true` | Dev mode active |

---

## Phase C: Test Infrastructure Improvements

### C1: Add `rstest` Parameterized Tests

Replace repetitive test functions with `#[rstest]` parameterized tests.

**`src/validation.rs` — SSRF blocked IPs:**

```rust
use rstest::rstest;

#[rstest]
#[case("127.0.0.1")]
#[case("10.0.0.1")]
#[case("10.255.255.255")]
#[case("172.16.0.1")]
#[case("172.31.255.255")]
#[case("192.168.0.1")]
#[case("192.168.255.255")]
#[case("169.254.0.1")]       // link-local
#[case("169.254.169.254")]   // cloud metadata
#[case("::1")]               // IPv6 loopback
#[case("fc00::1")]           // IPv6 unique-local
#[case("fe80::1")]           // IPv6 link-local
fn ssrf_blocks_private_ips(#[case] ip: &str) {
    let url = format!("http://{ip}/webhook");
    assert!(
        check_ssrf_url(&url, &["http", "https"]).is_err(),
        "SSRF should block {ip}"
    );
}

#[rstest]
#[case("93.184.216.34")]     // example.com
#[case("8.8.8.8")]           // Google DNS
#[case("2001:db8::1")]       // documentation IPv6
fn ssrf_allows_public_ips(#[case] ip: &str) {
    let url = format!("http://{ip}/webhook");
    assert!(
        check_ssrf_url(&url, &["http", "https"]).is_ok(),
        "SSRF should allow public IP {ip}"
    );
}
```

**`src/observe/proto.rs` — Severity mapping:**

```rust
#[rstest]
#[case(1, "trace")]
#[case(5, "debug")]
#[case(9, "info")]
#[case(13, "warn")]
#[case(17, "error")]
#[case(21, "fatal")]
fn severity_mapping(#[case] number: i32, #[case] expected: &str) {
    assert_eq!(severity_to_string(number), expected);
}
```

**`src/auth/user_type.rs` — Capability matrix:**

```rust
#[rstest]
#[case(UserType::Human, true, true, true)]
#[case(UserType::Agent, false, false, false)]
#[case(UserType::Service, false, false, true)]
fn user_type_capabilities(
    #[case] user_type: UserType,
    #[case] can_login: bool,
    #[case] can_spawn_agents: bool,
    #[case] requires_password: bool,
) {
    assert_eq!(user_type.can_login(), can_login);
    assert_eq!(user_type.can_spawn_agents(), can_spawn_agents);
    assert_eq!(user_type.requires_password(), requires_password);
}
```

### C2: Add `AuthUser::test_*` Constructors

**Modify: `src/auth/middleware.rs`** — add test-only constructors:

```rust
#[cfg(test)]
impl AuthUser {
    pub fn test_human(user_id: Uuid) -> Self {
        Self {
            user_id,
            user_name: "test_user".into(),
            ip_addr: Some("127.0.0.1".into()),
        }
    }

    pub fn test_with_name(user_id: Uuid, name: &str) -> Self {
        Self {
            user_id,
            user_name: name.into(),
            ip_addr: Some("127.0.0.1".into()),
        }
    }
}
```

---

## Phase D: Existing Test Refactoring

### D1: Strengthen Weak Assertions

Find all tests using `.is_err()` and replace with specific error matching:

**Files to modify:**
- `src/validation.rs` — ~15 assertions
- `src/secrets/engine.rs` — ~5 assertions
- `src/pipeline/definition.rs` — ~5 assertions
- `src/auth/password.rs` — ~2 assertions

**Pattern:**

```rust
// BEFORE
assert!(check_name("").is_err());

// AFTER
assert!(matches!(
    check_name(""),
    Err(ApiError::BadRequest(msg)) if msg.contains("name")
));
```

### D2: Fix Tautological Tests

| Test | File | Problem | Fix |
|------|------|---------|-----|
| `cache_key_deterministic` | `src/rbac/resolver.rs` | Same fn, same input = always equal | Test that different users produce different cache keys |
| `dev_master_key_is_deterministic` | `src/secrets/engine.rs` | Same fn, no randomness | Test key is non-zero AND works as valid encryption key |
| `internal_hides_details` | `src/error.rs` | Short-circuit assertion | Fix assertion (see B2) |

**`cache_key_deterministic` fix:**

```rust
// BEFORE
#[test]
fn cache_key_deterministic() {
    let a = cache_key(user_id, None);
    let b = cache_key(user_id, None);
    assert_eq!(a, b);
}

// AFTER
#[test]
fn cache_key_uniqueness() {
    let user_a = Uuid::new_v4();
    let user_b = Uuid::new_v4();
    let project = Uuid::new_v4();

    // Same user, same scope → same key
    assert_eq!(cache_key(user_a, None), cache_key(user_a, None));

    // Different users → different keys
    assert_ne!(cache_key(user_a, None), cache_key(user_b, None));

    // Same user, different scope → different keys
    assert_ne!(cache_key(user_a, None), cache_key(user_a, Some(project)));

    // Key format is predictable
    assert!(cache_key(user_a, None).starts_with("perms:"));
    assert!(cache_key(user_a, Some(project)).contains(&project.to_string()));
}
```

### D3: Activate `proptest` for Roundtrip Testing

**`src/rbac/types.rs` — Permission roundtrip:**

```rust
#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use super::Permission;

    // Generate arbitrary Permission variants
    fn arb_permission() -> impl Strategy<Value = Permission> {
        prop_oneof![
            Just(Permission::AdminUsers),
            Just(Permission::AdminRoles),
            Just(Permission::AdminConfig),
            Just(Permission::ProjectCreate),
            Just(Permission::ProjectRead),
            Just(Permission::ProjectWrite),
            Just(Permission::ProjectDelete),
            Just(Permission::DeployRead),
            Just(Permission::DeployPromote),
            Just(Permission::SecretRead),
            Just(Permission::SecretWrite),
            Just(Permission::ObserveRead),
            Just(Permission::ObserveWrite),
        ]
    }

    proptest! {
        #[test]
        fn permission_roundtrip(perm in arb_permission()) {
            let s = perm.as_str();
            let parsed = Permission::from_str(s).unwrap();
            prop_assert_eq!(perm, parsed);
        }

        #[test]
        fn permission_as_str_not_empty(perm in arb_permission()) {
            let s = perm.as_str();
            prop_assert!(!s.is_empty());
            prop_assert!(s.contains(':'), "permission string should contain ':'");
        }
    }
}
```

**`src/validation.rs` — LFS OID roundtrip:**

```rust
proptest! {
    #[test]
    fn valid_hex_oid_accepted(s in "[0-9a-f]{64}") {
        assert!(check_lfs_oid(&s).is_ok());
    }

    #[test]
    fn wrong_length_hex_rejected(s in "[0-9a-f]{1,63}|[0-9a-f]{65,128}") {
        assert!(check_lfs_oid(&s).is_err());
    }
}
```

---

## Implementation Sequence

| Phase | Scope | New Tests | Modified Tests |
|-------|-------|-----------|----------------|
| **B1** | Validation boundary tests | ~30 | 0 |
| **B2** | Error conversion tests | ~5 | 1 (fix tautological) |
| **B3** | Auth middleware edge cases | ~5 | 0 |
| **B4** | Secrets crypto boundaries | ~5 | 0 |
| **B5** | Pipeline pattern matching | ~8 | 0 |
| **B6** | Config edge cases | ~5 | 0 |
| **C1** | rstest parameterized tests | ~15 | ~10 (refactor to rstest) |
| **C2** | AuthUser test constructors | 0 | 0 (infra) |
| **D1** | Strengthen assertions | 0 | ~25 (replace .is_err()) |
| **D2** | Fix tautological tests | 0 | 3 |
| **D3** | proptest activation | ~5 | 0 |

**Total: ~78 new tests, ~39 modified tests**

---

## Files Modified

| File | Changes |
|------|---------|
| `src/validation.rs` | ~30 new boundary tests, ~15 strengthened assertions, proptest OID |
| `src/error.rs` | Fix tautological assertion, ~5 new conversion tests |
| `src/auth/middleware.rs` | ~5 new edge-case tests, `AuthUser::test_*` constructors |
| `src/secrets/engine.rs` | ~5 new boundary tests, ~5 strengthened assertions |
| `src/pipeline/definition.rs` | ~8 new pattern tests, ~5 strengthened assertions |
| `src/config.rs` | ~5 new config edge-case tests |
| `src/rbac/resolver.rs` | Fix tautological cache_key test |
| `src/rbac/types.rs` | proptest Permission roundtrip |
| `src/observe/proto.rs` | rstest severity/span-kind parameterized tests |
| `src/auth/user_type.rs` | rstest capability matrix (if applicable) |

---

## Verification

After each phase:
1. `just test-unit` — all existing + new unit tests pass
2. `just lint` — no clippy warnings in test code
3. `just fmt` — test code formatted

Final verification:
1. `just ci` — full gate passes
2. Count tests: `cargo nextest run --lib 2>&1 | tail -1` — should show ~340+ tests (up from 264)
3. Verify no `assert!(*.is_err())` remains without specific error matching (grep check)
4. Verify no `any(|_| false)` patterns remain (the tautological pattern)

---

## Risks and Mitigations

| Risk | Impact | Mitigation |
|------|--------|-----------|
| Changing assertions breaks tests | Reveals actual bugs (good) | Fix the bug, not the test |
| proptest finds unexpected inputs | Test failures | Investigate — these are real edge cases |
| rstest import issues | Compilation errors | Already in Cargo.toml dev-deps |
| Unicode validation behavior change | check_name("café") may change behavior | Document decision: ASCII-only names |
| Config tests env-dependent | Flaky in CI | Use `Config::test_default()` or controlled env vars |

---

## Estimated Scope

| Metric | Value |
|--------|-------|
| New unit tests | ~78 |
| Modified tests | ~39 |
| Files modified | ~10 |
| Estimated LOC changes | ~800 |
| New dependencies | 0 (all already in Cargo.toml) |
| Test count after | ~340+ (up from 264) |
