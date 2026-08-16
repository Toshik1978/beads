//! Errors raised by the `br remote` backends.
//!
//! Two shapes here are control flow rather than failure, and both are pinned
//! by the tests below: 429/5xx are the only retryable statuses, and a
//! `must-be-unique` body means the entity already exists — which is the
//! success case for a provisioning run racing another admin.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum RemoteError {
    #[error("remote returned HTTP {status}: {body}")]
    Http { status: u16, body: String },

    #[error("{entity} already exists")]
    AlreadyExists { entity: String },

    #[error("transport error: {0}")]
    Transport(String),

    #[error("remote configuration error: {0}")]
    Config(String),
}

impl RemoteError {
    /// Whether retrying the same request could plausibly succeed.
    ///
    /// Deliberately narrow: a 4xx other than 429 means the request was wrong,
    /// and retrying it just spends the rate-limit budget arriving at the same
    /// answer.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Http { status, .. } if *status == 429 || (500..600).contains(status))
    }

    /// Classify a non-2xx response.
    ///
    /// `entity` names what the caller was trying to create or modify, so an
    /// `AlreadyExists` reads usefully without the caller re-deriving it.
    #[must_use]
    pub fn from_response(status: u16, body: &str, entity: &str) -> Self {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(body)
            && value.get("error").and_then(serde_json::Value::as_str) == Some("must-be-unique")
        {
            return Self::AlreadyExists {
                entity: entity.to_string(),
            };
        }
        Self::Http {
            status,
            body: body.to_string(),
        }
    }
}

impl From<RemoteError> for crate::error::BeadsError {
    fn from(err: RemoteError) -> Self {
        Self::ExternalCommand {
            command: "br remote".to_string(),
            reason: err.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_429_and_5xx_are_retryable() {
        let retryable = [429_u16, 500, 502, 503, 504];
        for status in retryable {
            let err = RemoteError::Http {
                status,
                body: String::new(),
            };
            assert!(err.is_retryable(), "{status} must be retryable");
        }
        for status in [400_u16, 401, 403, 404, 409, 422] {
            let err = RemoteError::Http {
                status,
                body: String::new(),
            };
            assert!(!err.is_retryable(), "{status} must not be retryable");
        }
    }

    #[test]
    fn must_be_unique_body_becomes_already_exists() {
        let body = r#"{"error":"must-be-unique","error_field":"name",
            "error_description":"A field with the name 'Design' and the 'text' type already exists."}"#;
        let err = RemoteError::from_response(409, body, "custom field 'Design'");
        assert!(
            matches!(&err, RemoteError::AlreadyExists { entity } if entity == "custom field 'Design'"),
            "expected AlreadyExists, got {err:?}"
        );
        assert!(
            !err.is_retryable(),
            "an already-present entity must not be retried"
        );
    }

    #[test]
    fn an_unrelated_409_stays_http() {
        let err = RemoteError::from_response(409, r#"{"error":"conflict"}"#, "issue EM-1");
        assert!(
            matches!(err, RemoteError::Http { status: 409, .. }),
            "got {err:?}"
        );
    }
}
