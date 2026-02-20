use std::fmt;
use std::str::FromStr;

/// Discriminator for user accounts. Controls which auth methods and
/// capabilities are available to a given identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserType {
    Human,
    Agent,
    ServiceAccount,
}

impl UserType {
    /// Whether this user type can authenticate via password/session.
    pub fn can_login(self) -> bool {
        matches!(self, Self::Human)
    }

    /// Whether this user type can create agent sessions.
    #[allow(dead_code)] // consumed by 07-agent-orchestration
    pub fn can_spawn_agents(self) -> bool {
        matches!(self, Self::Human)
    }

    /// Whether this user type requires a password hash.
    pub fn requires_password(self) -> bool {
        matches!(self, Self::Human)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Agent => "agent",
            Self::ServiceAccount => "service_account",
        }
    }
}

impl fmt::Display for UserType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for UserType {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "human" => Ok(Self::Human),
            "agent" => Ok(Self::Agent),
            "service_account" => Ok(Self::ServiceAccount),
            other => anyhow::bail!("unknown user type: {other}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_all_types() {
        for ut in [UserType::Human, UserType::Agent, UserType::ServiceAccount] {
            let s = ut.as_str();
            let parsed: UserType = s.parse().unwrap();
            assert_eq!(ut, parsed, "roundtrip failed for {s}");
        }
    }

    #[test]
    fn can_login_only_human() {
        assert!(UserType::Human.can_login());
        assert!(!UserType::Agent.can_login());
        assert!(!UserType::ServiceAccount.can_login());
    }

    #[test]
    fn can_spawn_agents_only_human() {
        assert!(UserType::Human.can_spawn_agents());
        assert!(!UserType::Agent.can_spawn_agents());
        assert!(!UserType::ServiceAccount.can_spawn_agents());
    }

    #[test]
    fn requires_password_only_human() {
        assert!(UserType::Human.requires_password());
        assert!(!UserType::Agent.requires_password());
        assert!(!UserType::ServiceAccount.requires_password());
    }

    #[test]
    fn unknown_type_errors() {
        assert!("robot".parse::<UserType>().is_err());
    }

    #[test]
    fn serde_roundtrip() {
        let ut = UserType::ServiceAccount;
        let json = serde_json::to_string(&ut).unwrap();
        assert_eq!(json, "\"service_account\"");
        let parsed: UserType = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, ut);
    }

    #[test]
    fn display_matches_as_str() {
        for ut in [UserType::Human, UserType::Agent, UserType::ServiceAccount] {
            assert_eq!(ut.to_string(), ut.as_str());
        }
    }
}
