use std::fmt::Formatter;

use headers::Authorization;
use serde::Serialize;
use sync_types::headers::ConvexAdminAuthorization;

/// Encrypted system key
pub struct SystemKey(String);

impl SystemKey {
    // We're not using `Display` to avoid accidentally printing the key.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
/// Encrypted admin key
#[derive(Serialize, Clone)]
pub struct AdminKey(String);

impl std::fmt::Debug for AdminKey {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        f.write_str("AdminKey(*****)")
    }
}

impl AdminKey {
    pub fn new(key: String) -> Self {
        Self(key)
    }

    pub fn as_header(&self) -> anyhow::Result<Authorization<ConvexAdminAuthorization>> {
        Ok(Authorization(ConvexAdminAuthorization::from_admin_key(
            &self.0,
        )?))
    }

    // For a key like
    // "prod:some-depl-name123|sa67asd6a5da6d5:sd6f5sdf76dsf4ds6f4s68fd"
    // strips the initial "prod:" prefix.
    //
    // For a key like
    // "preview:team-slug:project-slug|sa67asd6a5da6d5:sd6f5sdf76dsf4ds6f4s68fd"
    // strips the entire prefix, returning just the key part
    // "sa67asd6a5da6d5:sd6f5sdf76dsf4ds6f4s68fd"
    pub fn remove_type_prefix(admin_key: &str) -> String {
        // check if key has an instance prefix
        let Some((instance_prefix, key_part)) = admin_key.split_once('|') else {
            return admin_key.to_string();
        };

        // check if instance prefix has a type prefix
        let Some((instance_type, instance_info)) = instance_prefix.split_once(':') else {
            return admin_key.to_string();
        };

        // if instance type is "preview" - return just the key part
        if instance_type.eq_ignore_ascii_case("preview") {
            return key_part.to_string();
        }

        // return instance info and key part
        format!("{}|{}", instance_info, key_part)
    }

    // We're not using `Display` to avoid accidentally printing the key.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn as_string(&self) -> String {
        self.0.to_string()
    }
}

impl SystemKey {
    pub fn new(key: String) -> Self {
        Self(key)
    }

    pub fn as_header(&self) -> anyhow::Result<Authorization<ConvexAdminAuthorization>> {
        Ok(Authorization(ConvexAdminAuthorization::from_admin_key(
            &self.0,
        )?))
    }
}

impl From<AdminKey> for AdminKeyParts {
    fn from(value: AdminKey) -> Self {
        split_admin_key(&value.0)
            .map(|(instance_name, encrypted_part)| {
                let (deployment_type_prefix, instance_name) = instance_name
                    .split_once(':')
                    .map(|(deployment_type_prefix, name)| (Some(deployment_type_prefix), name))
                    .unwrap_or((None, instance_name));
                AdminKeyParts {
                    deployment_type_prefix: deployment_type_prefix.map(|d| d.to_string()),
                    instance_name: Some(instance_name.to_string()),
                    encrypted_part: encrypted_part.to_string(),
                }
            })
            .unwrap_or(AdminKeyParts {
                deployment_type_prefix: None,
                instance_name: None,
                encrypted_part: value.0.to_string(),
            })
    }
}

impl TryFrom<AdminKeyParts> for AdminKey {
    type Error = anyhow::Error;

    fn try_from(value: AdminKeyParts) -> Result<Self, Self::Error> {
        let encrypted_part = value.encrypted_part;
        let key = match (value.deployment_type_prefix, value.instance_name) {
            (None, None) => encrypted_part,
            (None, Some(instance_identifier)) => format!("{instance_identifier}|{encrypted_part}"),
            (Some(_), None) => anyhow::bail!("Invalid admin key parts"),
            (Some(deployment_type_prefix), Some(instance_identifier)) => {
                format!("{deployment_type_prefix}:{instance_identifier}|{encrypted_part}")
            },
        };
        Ok(AdminKey::new(key))
    }
}

/// The different parts of 'prod:happy-animal-123|restofkey'
pub struct AdminKeyParts {
    pub deployment_type_prefix: Option<String>,
    pub instance_name: Option<String>,
    pub encrypted_part: String,
}

// TODO - encompass these floating methods into the `AdminKey` type

pub fn split_admin_key(admin_key: &str) -> Option<(&str, &str)> {
    admin_key.split_once('|')
}

pub fn format_admin_key(instance_name: &str, encrypted_part: &str) -> String {
    format!("{}|{}", instance_name, encrypted_part)
}

pub fn remove_type_prefix_from_admin_key(admin_key: &str) -> String {
    AdminKey::remove_type_prefix(admin_key)
}

// Dashboard adds a superficial prod: or dev: prefix
// for user's visibility to the admin key's instance name.
// CLI also adds this prefix to CONVEX_DEPLOYMENT env var.
// This method strips the prefix.
pub fn remove_type_prefix_from_instance_name(instance_name: &str) -> &str {
    instance_name
        .split_once(':')
        .map(|(_, name)| name)
        .unwrap_or(instance_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_remove_type_prefix() {
        assert_eq!(
            AdminKey::remove_type_prefix("prod:happy-animal-123|somesecret"),
            "happy-animal-123|somesecret"
        );
        assert_eq!(
            AdminKey::remove_type_prefix("prod:happy-animal-123|somesecret:somethingelse"),
            "happy-animal-123|somesecret:somethingelse"
        );
        assert_eq!(
            AdminKey::remove_type_prefix("dev:happy-animal-123|somesecret"),
            "happy-animal-123|somesecret"
        );
        assert_eq!(
            AdminKey::remove_type_prefix("happy-animal-123|somesecret"),
            "happy-animal-123|somesecret"
        );
        assert_eq!(
            AdminKey::remove_type_prefix("preview:sarah-shader:proset|somesecret"),
            "somesecret"
        );
        assert_eq!(
            AdminKey::remove_type_prefix("preview:sarah-shader:proset|somesecret:somethingelse"),
            "somesecret:somethingelse"
        );
        assert_eq!(AdminKey::remove_type_prefix("somesecret"), "somesecret");
        assert_eq!(
            AdminKey::remove_type_prefix("somesecret:somethingelse"),
            "somesecret:somethingelse"
        );
    }
}
