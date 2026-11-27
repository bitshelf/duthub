//! Authorization model for the transfer surface (fail-closed).
//!
//! Two fixed HTTP semantics (review #17): 401 = not authenticated (token
//! missing or unrecognized), 403 = authenticated but the scope does not cover
//! this request (board out of scope, declared length over the scope ceiling,
//! scope expired, or subject mismatch against the upload sidecar). The trait
//! is split into `authenticate` (401 path) and `authorize` (403 path) so the
//! HTTP middleware can order name validation (400) between the two.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// Current unix time in whole seconds.
#[must_use]
pub fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Current unix time in whole milliseconds — matches the precision fileloft's
/// `expires_at` carries, so the GC sweep and the tus-layer expiry check agree
/// instead of drifting by up to a second.
#[must_use]
pub fn now_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Which tus operation is being authorized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadOp {
    /// POST — create a new upload.
    Create,
    /// PATCH — append bytes.
    Write,
    /// HEAD — query the acknowledged offset.
    Query,
    /// DELETE — terminate the upload.
    Terminate,
}

/// What an authenticated caller may do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadScope {
    /// Identity bound into the upload sidecar at creation and re-checked on
    /// every PATCH/HEAD/DELETE (REVIEW:33).
    pub subject: String,
    /// Boards this scope may publish into (exact match).
    pub boards: Vec<String>,
    /// Per-upload `Upload-Length` ceiling.
    pub max_bytes: u64,
    /// Scope validity deadline (unix epoch seconds); `None` = no expiry.
    pub expires_at_epoch: Option<u64>,
}

/// Request context evaluated by [`UploadAuthorizer::authorize`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthContext<'a> {
    pub op: UploadOp,
    /// Board declared in the POST metadata (create) or recorded in the
    /// sidecar (PATCH/HEAD/DELETE).
    pub board: Option<&'a str>,
    /// `Upload-Length` declared on a POST.
    pub declared_len: Option<u64>,
    /// Subject recorded in the upload sidecar; `None` when the upload does
    /// not exist (let the tus layer answer 404 instead of leaking scope info).
    pub upload_subject: Option<&'a str>,
}

/// Authorization failures mapped one-to-one onto HTTP statuses by the router.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuthError {
    /// 401 — missing or invalid token.
    #[error("unauthorized")]
    Unauthorized,
    /// 403 — authenticated but not allowed.
    #[error("forbidden: {0}")]
    Forbidden(String),
}

/// Pluggable authorization seam. Token storage itself is out of scope for
/// stage one (`keyring` integration comes later); implementers provide their
/// own resolution.
pub trait UploadAuthorizer: Send + Sync {
    /// Resolve the Bearer token into a scope. `Err(Unauthorized)` → 401.
    fn authenticate(&self, token: Option<&str>) -> Result<UploadScope, AuthError>;

    /// Scope-level check after authentication. `Err(Forbidden)` → 403.
    fn authorize(&self, scope: &UploadScope, ctx: &AuthContext<'_>) -> Result<(), AuthError>;
}

/// Fail-closed default used when the host provides no authorizer: every
/// authenticated operation is rejected with 401, only OPTIONS stays anonymous.
#[derive(Debug)]
pub struct DenyAllAuthorizer;

impl UploadAuthorizer for DenyAllAuthorizer {
    fn authenticate(&self, _token: Option<&str>) -> Result<UploadScope, AuthError> {
        Err(AuthError::Unauthorized)
    }

    fn authorize(&self, _scope: &UploadScope, _ctx: &AuthContext<'_>) -> Result<(), AuthError> {
        Err(AuthError::Forbidden("denied".to_string()))
    }
}

/// Static token → scope table using SHA-256 digests and constant-time
/// comparison so response timing never leaks token content or length.
#[derive(Debug, Clone, Default)]
pub struct StaticTokenAuthorizer {
    entries: Vec<([u8; 32], UploadScope)>,
}

impl StaticTokenAuthorizer {
    #[must_use]
    pub fn new(scopes: HashMap<String, UploadScope>) -> Self {
        let entries = scopes
            .into_iter()
            .filter(|(token, _)| !token.is_empty())
            .map(|(token, scope)| {
                let digest: [u8; 32] = Sha256::digest(token.as_bytes()).into();
                (digest, scope)
            })
            .collect();
        Self { entries }
    }
}

impl UploadAuthorizer for StaticTokenAuthorizer {
    fn authenticate(&self, token: Option<&str>) -> Result<UploadScope, AuthError> {
        let Some(provided) = token.filter(|t| !t.is_empty()) else {
            return Err(AuthError::Unauthorized);
        };
        let digest: [u8; 32] = Sha256::digest(provided.as_bytes()).into();
        let mut matched: Option<&UploadScope> = None;
        for (expected, scope) in &self.entries {
            if bool::from(expected.as_slice().ct_eq(digest.as_slice())) {
                matched = Some(scope);
            }
        }
        matched.cloned().ok_or(AuthError::Unauthorized)
    }

    fn authorize(&self, scope: &UploadScope, ctx: &AuthContext<'_>) -> Result<(), AuthError> {
        if let Some(expires_at) = scope.expires_at_epoch
            && now_epoch_secs() > expires_at
        {
            return Err(AuthError::Forbidden("scope expired".to_string()));
        }
        match ctx.op {
            UploadOp::Create => {
                if !scope.boards.is_empty() {
                    match ctx.board {
                        Some(board) if scope.boards.iter().any(|allowed| allowed == board) => {}
                        Some(board) => {
                            return Err(AuthError::Forbidden(format!(
                                "board {board:?} not allowed"
                            )));
                        }
                        None => {
                            return Err(AuthError::Forbidden(
                                "board metadata required for board-scoped token".to_string(),
                            ));
                        }
                    }
                } else if let Some(board) = ctx.board {
                    return Err(AuthError::Forbidden(format!("board {board:?} not allowed")));
                }
                if let Some(len) = ctx.declared_len
                    && len > scope.max_bytes
                {
                    return Err(AuthError::Forbidden(
                        "declared length exceeds scope ceiling".to_string(),
                    ));
                }
            }
            UploadOp::Write | UploadOp::Query | UploadOp::Terminate => {
                if let Some(recorded) = ctx.upload_subject
                    && recorded != scope.subject
                {
                    return Err(AuthError::Forbidden("subject mismatch".to_string()));
                }
                if let Some(board) = ctx.board
                    && !scope.boards.iter().any(|allowed| allowed == board)
                {
                    return Err(AuthError::Forbidden(format!("board {board:?} not allowed")));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(subject: &str, boards: &[&str], max: u64, expires: Option<u64>) -> UploadScope {
        UploadScope {
            subject: subject.to_string(),
            boards: boards.iter().map(|b| (*b).to_string()).collect(),
            max_bytes: max,
            expires_at_epoch: expires,
        }
    }

    #[test]
    fn static_token_authenticate_maps_to_401_and_200() {
        let auth = StaticTokenAuthorizer::new(HashMap::from([(
            "tok".to_string(),
            scope("agent-a", &["board1"], 100, None),
        )]));
        assert!(auth.authenticate(Some("tok")).is_ok());
        assert_eq!(auth.authenticate(None), Err(AuthError::Unauthorized));
        assert_eq!(
            auth.authenticate(Some("nope")),
            Err(AuthError::Unauthorized)
        );
    }

    #[test]
    fn deny_all_never_authenticates() {
        let auth = DenyAllAuthorizer;
        assert_eq!(auth.authenticate(Some("tok")), Err(AuthError::Unauthorized));
    }

    #[test]
    fn create_rejects_foreign_board_and_oversized_declared_length() {
        let auth = StaticTokenAuthorizer::new(HashMap::new());
        let agent = scope("agent-a", &["board1"], 100, None);
        let foreign = AuthContext {
            op: UploadOp::Create,
            board: Some("board2"),
            declared_len: Some(50),
            upload_subject: None,
        };
        assert!(auth.authorize(&agent, &foreign).is_err());
        let missing_board = AuthContext {
            op: UploadOp::Create,
            board: None,
            declared_len: Some(50),
            upload_subject: None,
        };
        assert!(auth.authorize(&agent, &missing_board).is_err());
        let oversized = AuthContext {
            op: UploadOp::Create,
            board: Some("board1"),
            declared_len: Some(101),
            upload_subject: None,
        };
        assert!(auth.authorize(&agent, &oversized).is_err());
    }

    #[test]
    fn resource_ops_reject_subject_mismatch_and_expired_scopes() {
        let auth = StaticTokenAuthorizer::new(HashMap::new());
        let agent = scope("agent-a", &["board1"], 100, None);
        let other_subject = AuthContext {
            op: UploadOp::Write,
            board: Some("board1"),
            declared_len: None,
            upload_subject: Some("agent-b"),
        };
        assert_eq!(
            auth.authorize(&agent, &other_subject),
            Err(AuthError::Forbidden("subject mismatch".to_string()))
        );
        // Missing sidecar subject (unknown upload) passes so the tus layer
        // can answer 404.
        let unknown = AuthContext {
            op: UploadOp::Query,
            board: None,
            declared_len: None,
            upload_subject: None,
        };
        assert!(auth.authorize(&agent, &unknown).is_ok());

        let expired = scope("agent-a", &["board1"], 100, Some(now_epoch_secs() - 10));
        let ctx = AuthContext {
            op: UploadOp::Terminate,
            board: Some("board1"),
            declared_len: None,
            upload_subject: Some("agent-a"),
        };
        assert_eq!(
            auth.authorize(&expired, &ctx),
            Err(AuthError::Forbidden("scope expired".to_string()))
        );
    }
}
