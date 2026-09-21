//! Typed extraction of the headers the contract requires.

use axum::extract::FromRequestParts;
use http::request::Parts;
use sandbox_protocol::IdempotencyKey;

use crate::problem::Problem;

/// The `Idempotency-Key` header, validated.
///
/// Required on every mutation. A caller who omits it gets a `400` rather than
/// a silently non-idempotent request, because the failure mode of the latter
/// is running someone's command twice.
#[derive(Debug, Clone)]
pub struct RequestKey(pub IdempotencyKey);

impl<S: Send + Sync> FromRequestParts<S> for RequestKey {
    type Rejection = Problem;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let value = parts
            .headers
            .get("idempotency-key")
            .ok_or(Problem::BadRequest(
                "the Idempotency-Key header is required",
            ))?
            .to_str()
            .map_err(|_| Problem::BadRequest("the Idempotency-Key header is not valid ASCII"))?;

        IdempotencyKey::parse(value)
            .map(Self)
            .map_err(|_| Problem::BadRequest("the Idempotency-Key header is not a valid key"))
    }
}
