use axum::Extension;
use axum::extract::{ConnectInfo, FromRequestParts, Query};
use axum::http::header::USER_AGENT;
use axum::http::request::Parts;
use serde::Deserialize;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::net::SocketAddr;
use tracing::{debug, error};

use crate::server::error::Error;
use crate::server::services::edge_services::EdgeServices;

#[derive(Deserialize)]
struct SignedUrlQuery {
    sig: Option<String>,
    exp: Option<String>,
    client: Option<String>, // client identifier (hashed IP + user-agent)
}

pub struct EdgeAuthentication(pub String, pub EdgeServices);

/// generates a client identifier from IP address and user-agent
pub fn generate_client_id(ip: Option<&str>, user_agent: Option<&str>) -> String {
    let mut hasher = DefaultHasher::new();
    ip.unwrap_or("unknown").hash(&mut hasher);
    user_agent.unwrap_or("unknown").hash(&mut hasher);
    format!("{:x}", hasher.finish())
}

/// edge authentication extractor - no database required
/// uses stateless signatures with IP + user-agent hashing
impl<S> FromRequestParts<S> for EdgeAuthentication
where
    S: Send + Sync,
{
    type Rejection = Error;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let Extension(services): Extension<EdgeServices> =
            Extension::from_request_parts(parts, state)
                .await
                .map_err(|err| Error::InternalServerErrorWithContext(err.to_string()))?;

        let user_agent = parts
            .headers
            .get(USER_AGENT)
            .and_then(|h| h.to_str().ok())
            .map(|s| s.to_string());

        // try to get client IP from X-Forwarded-For, X-Real-IP, or connection info
        let client_ip = parts
            .headers
            .get("x-forwarded-for")
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.split(',').next())
            .map(|s| s.trim().to_string())
            .or_else(|| {
                parts
                    .headers
                    .get("x-real-ip")
                    .and_then(|h| h.to_str().ok())
                    .map(|s| s.to_string())
            })
            .or_else(|| {
                parts
                    .extensions
                    .get::<ConnectInfo<SocketAddr>>()
                    .map(|ci| ci.0.ip().to_string())
            });

        let client_id = generate_client_id(client_ip.as_deref(), user_agent.as_deref());
        debug!(
            "Generated client_id: {} from IP: {:?}",
            client_id, client_ip
        );

        // check for signed URL parameters
        let Query(query): Query<SignedUrlQuery> = Query::from_request_parts(parts, state)
            .await
            .unwrap_or(Query(SignedUrlQuery {
                sig: None,
                exp: None,
                client: None,
            }));

        // verify
        if let (Some(sig), Some(exp_str)) = (query.sig.as_ref(), query.exp.as_ref()) {
            let expiry = exp_str.parse::<i64>().map_err(|_| {
                error!("invalid expiry timestamp");
                Error::Unauthorized
            })?;

            let url_param = parts
                .uri
                .query()
                .and_then(|q| {
                    q.split('&')
                        .find(|param| param.starts_with("url="))
                        .and_then(|param| param.strip_prefix("url="))
                })
                .ok_or_else(|| {
                    error!("missing url parameter in signed URL");
                    Error::Unauthorized
                })?;

            // use the client_id from the query (what was used to generate the signature)
            // or fall back to the current client_id
            let signature_client_id = query.client.as_deref().unwrap_or(&client_id);

            if !services.signature_util.verify_signature(
                signature_client_id,
                expiry,
                url_param,
                sig,
            ) {
                error!(
                    "Signature invalid - url: {}, client: {}, expiry: {}",
                    url_param, signature_client_id, expiry
                );
                return Err(Error::Unauthorized);
            }

            debug!("Signature verified for client: {}", signature_client_id);
        }

        // allow requests through without strict auth
        // rate limiting can still be applied based on client_id
        Ok(EdgeAuthentication(client_id, services))
    }
}
