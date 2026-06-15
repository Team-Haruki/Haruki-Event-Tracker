use reqwest::StatusCode;
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT};
use serde::Deserialize;
use sonic_rs::JsonValueTrait;

use crate::config::ToolboxConfig;
use crate::model::enums::SekaiServerRegion;

#[derive(Clone)]
pub struct PrivateLookupVerifier {
    base_url: String,
    auth_proxy_secret: Option<String>,
    client: reqwest::Client,
}

#[derive(Debug, thiserror::Error)]
pub enum PrivateLookupError {
    #[error("private lookup verifier is not configured")]
    NotConfigured,
    #[error("login required")]
    Unauthorized,
    #[error("bound account required")]
    Forbidden,
    #[error("toolbox private lookup verifier request failed")]
    Upstream,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolboxResponse<T> {
    #[allow(dead_code)]
    status: Option<i64>,
    #[allow(dead_code)]
    message: Option<String>,
    updated_data: Option<T>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolboxUserSettings {
    kratos_identity_id: Option<String>,
    #[serde(default)]
    game_account_bindings: Vec<ToolboxGameAccountBinding>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolboxGameAccountBinding {
    server: String,
    user_id: sonic_rs::Value,
}

impl PrivateLookupVerifier {
    pub fn from_config(config: &ToolboxConfig) -> Option<Self> {
        let base_url = normalize_base_url(&config.base_url)?;
        let auth_proxy_secret = normalize_optional_header_value(&config.auth_proxy_secret);
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        if let Ok(value) = HeaderValue::from_str(config.authorization.trim()) {
            if !value.as_bytes().is_empty() {
                headers.insert(AUTHORIZATION, value);
            }
        }
        if let Ok(value) = HeaderValue::from_str(config.user_agent.trim()) {
            if !value.as_bytes().is_empty() {
                headers.insert(USER_AGENT, value);
            }
        }

        let client = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .ok()?;
        Some(Self {
            base_url,
            auth_proxy_secret,
            client,
        })
    }

    pub async fn verify_bound_user(
        &self,
        subject: &str,
        owner: Option<&str>,
        server: SekaiServerRegion,
        user_id: &str,
    ) -> Result<(), PrivateLookupError> {
        let subject = subject.trim();
        if subject.is_empty() {
            return Err(PrivateLookupError::Unauthorized);
        }
        let owner = owner.map(str::trim).filter(|value| !value.is_empty());
        if let Some(owner) = owner {
            if owner != subject {
                return Err(PrivateLookupError::Forbidden);
            }
        }

        let url = format!("{}/api/user/me", self.base_url);
        let mut request = self.client.get(url).header("X-Kratos-Identity-Id", subject);
        if let Some(secret) = &self.auth_proxy_secret {
            request = request.header("X-Auth-Proxy-Secret", secret);
        }
        let response = request
            .send()
            .await
            .map_err(|_| PrivateLookupError::Upstream)?;
        let status = response.status();
        if status == StatusCode::UNAUTHORIZED {
            return Err(PrivateLookupError::Unauthorized);
        }
        if !status.is_success() {
            return Err(PrivateLookupError::Upstream);
        }

        let body = response
            .bytes()
            .await
            .map_err(|_| PrivateLookupError::Upstream)?;
        let response = sonic_rs::from_slice::<ToolboxResponse<ToolboxUserSettings>>(&body)
            .map_err(|_| PrivateLookupError::Upstream)?;
        let Some(settings) = response.updated_data else {
            return Err(PrivateLookupError::Upstream);
        };
        if settings
            .kratos_identity_id
            .as_deref()
            .map(str::trim)
            .is_some_and(|value| !value.is_empty() && value != subject)
        {
            return Err(PrivateLookupError::Forbidden);
        }

        let requested_server = server.to_string();
        let requested_user_id = user_id.trim();
        let bound = settings.game_account_bindings.iter().any(|binding| {
            binding
                .server
                .trim()
                .eq_ignore_ascii_case(&requested_server)
                && normalize_toolbox_user_id(&binding.user_id)
                    .as_deref()
                    .is_some_and(|value| value == requested_user_id)
        });

        if bound {
            Ok(())
        } else {
            Err(PrivateLookupError::Forbidden)
        }
    }
}

fn normalize_base_url(value: &str) -> Option<String> {
    let trimmed = value.trim().trim_end_matches('/');
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

fn normalize_optional_header_value(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn normalize_toolbox_user_id(value: &sonic_rs::Value) -> Option<String> {
    value
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .or_else(|| value.as_i64().map(|value| value.to_string()))
        .or_else(|| value.as_u64().map(|value| value.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::http::HeaderMap;
    use axum::routing::get;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    #[tokio::test]
    async fn verifies_bound_account_and_forwards_auth_proxy_headers() {
        let headers_seen = Arc::new(Mutex::new(Vec::new()));
        let base_url = spawn_toolbox(
            r#"{"updatedData":{"kratosIdentityId":"identity-1","gameAccountBindings":[{"server":"jp","userId":123456789}]}}"#,
            headers_seen.clone(),
        )
        .await;
        let verifier = PrivateLookupVerifier::from_config(&ToolboxConfig {
            base_url,
            auth_proxy_secret: "shared-secret".to_owned(),
            authorization: String::new(),
            user_agent: "verifier-test".to_owned(),
        })
        .expect("verifier");

        verifier
            .verify_bound_user(
                "identity-1",
                Some("identity-1"),
                SekaiServerRegion::Jp,
                "123456789",
            )
            .await
            .expect("bound account should verify");

        let headers = headers_seen.lock().await;
        assert_eq!(headers[0].0, "identity-1");
        assert_eq!(headers[0].1, "shared-secret");
    }

    #[tokio::test]
    async fn rejects_owner_mismatch_before_toolbox_lookup() {
        let headers_seen = Arc::new(Mutex::new(Vec::new()));
        let base_url = spawn_toolbox(
            r#"{"updatedData":{"kratosIdentityId":"identity-1","gameAccountBindings":[{"server":"jp","userId":123456789}]}}"#,
            headers_seen.clone(),
        )
        .await;
        let verifier = PrivateLookupVerifier::from_config(&ToolboxConfig {
            base_url,
            auth_proxy_secret: String::new(),
            authorization: String::new(),
            user_agent: String::new(),
        })
        .expect("verifier");

        let err = verifier
            .verify_bound_user(
                "identity-1",
                Some("identity-2"),
                SekaiServerRegion::Jp,
                "123456789",
            )
            .await
            .expect_err("owner mismatch should reject");

        assert!(matches!(err, PrivateLookupError::Forbidden));
        assert!(headers_seen.lock().await.is_empty());
    }

    #[tokio::test]
    async fn rejects_unbound_account() {
        let headers_seen = Arc::new(Mutex::new(Vec::new()));
        let base_url = spawn_toolbox(
            r#"{"updatedData":{"kratosIdentityId":"identity-1","gameAccountBindings":[{"server":"en","userId":123456789}]}}"#,
            headers_seen,
        )
        .await;
        let verifier = PrivateLookupVerifier::from_config(&ToolboxConfig {
            base_url,
            auth_proxy_secret: String::new(),
            authorization: String::new(),
            user_agent: String::new(),
        })
        .expect("verifier");

        let err = verifier
            .verify_bound_user("identity-1", None, SekaiServerRegion::Jp, "123456789")
            .await
            .expect_err("unbound account should reject");

        assert!(matches!(err, PrivateLookupError::Forbidden));
    }

    async fn spawn_toolbox(
        body: &'static str,
        headers_seen: Arc<Mutex<Vec<(String, String)>>>,
    ) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test listener");
        let addr = listener.local_addr().expect("local addr");
        let app = Router::new().route(
            "/api/user/me",
            get(move |headers: HeaderMap| {
                let headers_seen = headers_seen.clone();
                async move {
                    let identity = headers
                        .get("x-kratos-identity-id")
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or("")
                        .to_owned();
                    let secret = headers
                        .get("x-auth-proxy-secret")
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or("")
                        .to_owned();
                    headers_seen.lock().await.push((identity, secret));
                    (
                        axum::http::StatusCode::OK,
                        [(axum::http::header::CONTENT_TYPE, "application/json")],
                        body,
                    )
                }
            }),
        );
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve test app");
        });
        format!("http://{addr}")
    }
}
