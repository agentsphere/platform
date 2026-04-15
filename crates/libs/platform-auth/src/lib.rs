// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! Authentication, authorization, token hashing, rate limiting, and permission resolver.

pub mod auth_user;
pub mod extract;
pub mod lookup;
pub mod passkey;
pub mod password;
pub mod rate_limit;
pub mod resolver;
pub mod token;
pub mod workspace;

// Re-export key types at crate root.
pub use auth_user::AuthUser;
pub use extract::{cidr_matches, extract_bearer_token, extract_ip, extract_session_cookie};
pub use lookup::{SessionAuthLookup, TokenAuthLookup, lookup_api_token, lookup_session};
pub use password::{dummy_hash, hash_password, verify_password};
pub use rate_limit::check_rate;
pub use resolver::{PgPermissionChecker, set_cache_ttl};
pub use token::{generate_api_token, generate_session_token, hash_token};
pub use workspace::PgWorkspaceMembershipChecker;
