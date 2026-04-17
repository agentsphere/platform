// Copyright (c) 2026 Steven Hooker. Exclusively licensed to and distributed by AgentSphere GmbH.
// SPDX-License-Identifier: BUSL-1.1

//! Ephemeral in-memory state for agent secret requests.
//!
//! When an agent needs a secret (e.g. API key), it creates a pending request.
//! The UI renders a modal for the user to enter values. Once submitted, the
//! request is marked complete. Requests expire after 5 minutes.

use std::time::Instant;

use serde::Serialize;
use uuid::Uuid;

/// How long a secret request stays valid before timing out.
const TIMEOUT_SECS: u64 = 300; // 5 minutes

/// Maximum pending requests per agent session.
pub const MAX_PENDING_PER_SESSION: usize = 10;

#[derive(Debug, Clone, Serialize)]
pub struct SecretRequest {
    pub id: Uuid,
    pub project_id: Uuid,
    pub session_id: Uuid,
    pub name: String,
    pub description: String,
    pub environments: Vec<String>,
    pub status: SecretRequestStatus,
    #[serde(skip)]
    pub created_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretRequestStatus {
    Pending,
    Completed,
    TimedOut,
}

impl SecretRequest {
    /// Returns true if the request has exceeded the timeout window.
    pub fn is_timed_out(&self) -> bool {
        self.created_at.elapsed() > std::time::Duration::from_secs(TIMEOUT_SECS)
    }

    /// Returns the effective status, accounting for timeout.
    pub fn effective_status(&self) -> SecretRequestStatus {
        if self.status == SecretRequestStatus::Pending && self.is_timed_out() {
            SecretRequestStatus::TimedOut
        } else {
            self.status
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_request_is_not_timed_out() {
        let req = SecretRequest {
            id: Uuid::new_v4(),
            project_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            name: "API_KEY".into(),
            description: "test".into(),
            environments: vec!["production".into()],
            status: SecretRequestStatus::Pending,
            created_at: Instant::now(),
        };
        assert!(!req.is_timed_out());
        assert_eq!(req.effective_status(), SecretRequestStatus::Pending);
    }

    #[test]
    fn completed_request_stays_completed_even_if_old() {
        let req = SecretRequest {
            id: Uuid::new_v4(),
            project_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            name: "DB_URL".into(),
            description: "test".into(),
            environments: vec![],
            status: SecretRequestStatus::Completed,
            created_at: Instant::now()
                .checked_sub(std::time::Duration::from_secs(600))
                .unwrap(),
        };
        assert_eq!(req.effective_status(), SecretRequestStatus::Completed);
    }

    #[test]
    fn pending_request_times_out_after_5_min() {
        let req = SecretRequest {
            id: Uuid::new_v4(),
            project_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            name: "KEY".into(),
            description: String::new(),
            environments: vec![],
            status: SecretRequestStatus::Pending,
            created_at: Instant::now()
                .checked_sub(std::time::Duration::from_secs(301))
                .unwrap(),
        };
        assert!(req.is_timed_out());
        assert_eq!(req.effective_status(), SecretRequestStatus::TimedOut);
    }

    #[test]
    fn status_serialization() {
        assert_eq!(
            serde_json::to_string(&SecretRequestStatus::Pending).unwrap(),
            "\"pending\""
        );
        assert_eq!(
            serde_json::to_string(&SecretRequestStatus::Completed).unwrap(),
            "\"completed\""
        );
        assert_eq!(
            serde_json::to_string(&SecretRequestStatus::TimedOut).unwrap(),
            "\"timed_out\""
        );
    }
}
