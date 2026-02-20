use std::fmt;
use std::str::FromStr;

/// All platform permissions. Must match the `name` column in the `permissions` table
/// seeded by `store::bootstrap`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Permission {
    ProjectRead,
    ProjectWrite,
    ProjectDelete,
    AgentRun,
    DeployRead,
    DeployPromote,
    ObserveRead,
    ObserveWrite,
    AlertManage,
    SecretRead,
    SecretWrite,
    AdminUsers,
    AdminDelegate,
}

impl Permission {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ProjectRead => "project:read",
            Self::ProjectWrite => "project:write",
            Self::ProjectDelete => "project:delete",
            Self::AgentRun => "agent:run",
            Self::DeployRead => "deploy:read",
            Self::DeployPromote => "deploy:promote",
            Self::ObserveRead => "observe:read",
            Self::ObserveWrite => "observe:write",
            Self::AlertManage => "alert:manage",
            Self::SecretRead => "secret:read",
            Self::SecretWrite => "secret:write",
            Self::AdminUsers => "admin:users",
            Self::AdminDelegate => "admin:delegate",
        }
    }
}

impl fmt::Display for Permission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Permission {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "project:read" => Ok(Self::ProjectRead),
            "project:write" => Ok(Self::ProjectWrite),
            "project:delete" => Ok(Self::ProjectDelete),
            "agent:run" => Ok(Self::AgentRun),
            "deploy:read" => Ok(Self::DeployRead),
            "deploy:promote" => Ok(Self::DeployPromote),
            "observe:read" => Ok(Self::ObserveRead),
            "observe:write" => Ok(Self::ObserveWrite),
            "alert:manage" => Ok(Self::AlertManage),
            "secret:read" => Ok(Self::SecretRead),
            "secret:write" => Ok(Self::SecretWrite),
            "admin:users" => Ok(Self::AdminUsers),
            "admin:delegate" => Ok(Self::AdminDelegate),
            other => anyhow::bail!("unknown permission: {other}"),
        }
    }
}

impl serde::Serialize for Permission {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> serde::Deserialize<'de> for Permission {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::from_str(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_PERMISSIONS: &[Permission] = &[
        Permission::ProjectRead,
        Permission::ProjectWrite,
        Permission::ProjectDelete,
        Permission::AgentRun,
        Permission::DeployRead,
        Permission::DeployPromote,
        Permission::ObserveRead,
        Permission::ObserveWrite,
        Permission::AlertManage,
        Permission::SecretRead,
        Permission::SecretWrite,
        Permission::AdminUsers,
        Permission::AdminDelegate,
    ];

    #[test]
    fn roundtrip_all_permissions() {
        for perm in ALL_PERMISSIONS {
            let s = perm.as_str();
            let parsed: Permission = s.parse().unwrap();
            assert_eq!(*perm, parsed, "roundtrip failed for {s}");
        }
    }

    #[test]
    fn all_permissions_counted() {
        assert_eq!(ALL_PERMISSIONS.len(), 13);
    }

    #[test]
    fn unknown_permission_errors() {
        assert!("foo:bar".parse::<Permission>().is_err());
    }

    #[test]
    fn serde_roundtrip() {
        let perm = Permission::ProjectRead;
        let json = serde_json::to_string(&perm).unwrap();
        assert_eq!(json, "\"project:read\"");
        let parsed: Permission = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, perm);
    }

    #[test]
    fn serde_roundtrip_all_permissions() {
        for perm in ALL_PERMISSIONS {
            let json = serde_json::to_string(perm).unwrap();
            let parsed: Permission = serde_json::from_str(&json).unwrap();
            assert_eq!(
                *perm,
                parsed,
                "serde roundtrip failed for {}",
                perm.as_str()
            );
        }
    }

    #[test]
    fn display_matches_as_str() {
        for perm in ALL_PERMISSIONS {
            assert_eq!(perm.to_string(), perm.as_str());
        }
    }

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        fn arb_permission() -> impl Strategy<Value = Permission> {
            (0..ALL_PERMISSIONS.len()).prop_map(|i| ALL_PERMISSIONS[i])
        }

        proptest! {
            #[test]
            fn permission_as_str_from_str_roundtrip(perm in arb_permission()) {
                let s = perm.as_str();
                let parsed: Permission = s.parse().unwrap();
                prop_assert_eq!(perm, parsed);
            }

            #[test]
            fn permission_serde_roundtrip(perm in arb_permission()) {
                let json = serde_json::to_string(&perm).unwrap();
                let parsed: Permission = serde_json::from_str(&json).unwrap();
                prop_assert_eq!(perm, parsed);
            }
        }
    }
}
