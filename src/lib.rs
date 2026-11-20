use google_cloud_secretmanager_v1::client::SecretManagerService;
use serde::Deserialize;
use serde::Deserializer;
use serde::de::MapAccess;
use serde::de::Visitor;
use serde::de::{self};
use std::path::Path;

use montclair_observability::TError;

#[derive(Debug, thiserror::Error)]
#[error("ConfigSourceError: {0}")]
pub struct ConfigSourceError(String);

/// A configuration source defined in TOML.
///
/// # Examples
///
/// ## Literal string
/// ```toml
/// # TOML
/// postgres_url = "postgres://..."
/// ```
///
/// ```rs
/// // Rust
/// ConfigSource::Literal { value: "postgres://..." }
/// ```
///
/// ## Environment variable
/// ```toml
/// # TOML
/// postgres_url = { env = "POSTGRES_URL" }
/// ```
///
/// ```rs
/// // Rust
/// ConfigSource::Env { name: "POSTGRES_URL" }
/// ```
///
/// ## Google Cloud Secrets Manager
/// ```toml
/// # TOML
/// postgres_url = { project_id = "123", secret_id = "123", version_id = "123" }
/// ```
///
/// ```rs
/// // Rust
/// ConfigSource::GoogleCloudSecretsManager { project_id: "123", secret_id: "123", version_id: "123" }
/// ```
pub enum ConfigSource {
    Literal {
        value: String,
    },
    Env {
        name: String,
    },
    GoogleCloudSecretManager {
        project_id: String,
        secret_id: String,
        version_id: String,
    },
}

impl ConfigSource {
    pub async fn resolve(&self) -> Result<String, TError> {
        match self {
            ConfigSource::Literal { value } => Ok(value.clone()),
            ConfigSource::Env { name } => match std::env::var(name) {
                Ok(v) => Ok(v),
                Err(std::env::VarError::NotPresent) => {
                    Err(ConfigSourceError(format!("Environment variable {name} is not set")).into())
                }
                Err(std::env::VarError::NotUnicode(v)) => Err(ConfigSourceError(format!(
                    "Environment variable {name} contains non-unicode characters: {v:?}"
                ))
                .into()),
            },
            ConfigSource::GoogleCloudSecretManager {
                project_id,
                secret_id,
                version_id,
            } => {
                let client = SecretManagerService::builder().build().await.map_err(|e| {
                    ConfigSourceError(format!("GoogleCloudSecretsManager: build error: {e:?}"))
                })?;

                let name =
                    format!("projects/{project_id}/secrets/{secret_id}/versions/{version_id}");

                let response = client
                    .access_secret_version()
                    .set_name(&name)
                    .send()
                    .await
                    .map_err(|e| {
                        ConfigSourceError(format!("GoogleCloudSecretsManager: access error: {e:?}"))
                    })?;

                let payload = match response.payload {
                    Some(payload) => payload,
                    None => {
                        return Err(ConfigSourceError(format!(
                            "GoogleCloudSecretsManager: no payload for [{name}]"
                        ))
                        .into());
                    }
                };

                let v = String::from_utf8(payload.data.to_vec())?;

                Ok(v)
            }
        }
    }
}

impl std::fmt::Debug for ConfigSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Literal { value } => f.write_str(value),
            Self::Env { name } => f.debug_struct("Env").field("name", name).finish(),
            Self::GoogleCloudSecretManager {
                project_id,
                secret_id,
                version_id,
            } => f
                .debug_struct("GoogleCloudSecretsManager")
                .field("project_id", project_id)
                .field("secret_id", secret_id)
                .field("version_id", version_id)
                .finish(),
        }
    }
}

impl<'de> Deserialize<'de> for ConfigSource {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Unknown keys in the table produce serde's "unknown variant `x`, expected one of ..."
        #[derive(Deserialize)]
        #[serde(field_identifier, rename_all = "snake_case")]
        enum Field {
            Env,
            ProjectId,
            SecretId,
            VersionId,
        }

        struct V;

        impl<'de> Visitor<'de> for V {
            type Value = ConfigSource;

            // Used by serde for wrong-type errors, e.g. "invalid type: integer `5`, expected ..."
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(
                    "a string literal, `{ env = \"NAME\" }`, or \
                     `{ project_id = \"..\", secret_id = \"..\", version_id = \"..\" }`",
                )
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(ConfigSource::Literal {
                    value: v.to_owned(),
                })
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                fn set<'de, A: MapAccess<'de>>(
                    slot: &mut Option<String>,
                    name: &'static str,
                    map: &mut A,
                ) -> Result<(), A::Error> {
                    if slot.is_some() {
                        return Err(de::Error::duplicate_field(name));
                    }
                    *slot = Some(map.next_value()?);
                    Ok(())
                }

                let (mut env, mut project_id, mut secret_id, mut version_id) =
                    (None, None, None, None);
                while let Some(key) = map.next_key::<Field>()? {
                    match key {
                        Field::Env => set(&mut env, "env", &mut map)?,
                        Field::ProjectId => set(&mut project_id, "project_id", &mut map)?,
                        Field::SecretId => set(&mut secret_id, "secret_id", &mut map)?,
                        Field::VersionId => set(&mut version_id, "version_id", &mut map)?,
                    }
                }

                match (env, project_id, secret_id, version_id) {
                    (Some(name), None, None, None) => {
                        if name.is_empty() || name.contains(['=', '\0']) {
                            return Err(de::Error::custom(format!(
                                "`env`: invalid environment variable name {name:?}"
                            )));
                        }
                        Ok(ConfigSource::Env { name })
                    }
                    (None, Some(project_id), Some(secret_id), Some(version_id)) => {
                        // Reject '/' so values can't alter the resource path
                        // `projects/*/secrets/*/versions/*`.
                        for (field, v) in [
                            ("project_id", &project_id),
                            ("secret_id", &secret_id),
                            ("version_id", &version_id),
                        ] {
                            if v.is_empty() || v.contains('/') {
                                return Err(de::Error::custom(format!(
                                    "`{field}` must be non-empty and must not contain '/'"
                                )));
                            }
                        }
                        Ok(ConfigSource::GoogleCloudSecretManager {
                            project_id,
                            secret_id,
                            version_id,
                        })
                    }
                    (None, None, None, None) => Err(de::Error::custom(
                        "empty table: expected `env` or `project_id`/`secret_id`/`version_id`",
                    )),
                    (Some(_), ..) => Err(de::Error::custom(
                        "`env` cannot be combined with `project_id`, `secret_id` or `version_id`",
                    )),
                    (None, p, s, v) => Err(de::Error::missing_field(if p.is_none() {
                        "project_id"
                    } else if s.is_none() {
                        "secret_id"
                    } else {
                        debug_assert!(v.is_none());
                        "version_id"
                    })),
                }
            }
        }

        // TOML is self-describing, so `deserialize_any` dispatches to visit_str / visit_map.
        deserializer.deserialize_any(V)
    }
}

#[async_trait::async_trait]
pub trait ConfigSourceExt {
    async fn resolve(&self, field: &str) -> Result<String, TError>;
}

#[async_trait::async_trait]
impl ConfigSourceExt for Option<ConfigSource> {
    async fn resolve(&self, field: &str) -> Result<String, TError> {
        match self {
            Some(inner) => inner.resolve().await,
            None => Err(ConfigSourceError(format!("config field [{field}] not defined")).into()),
        }
    }
}
