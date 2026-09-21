//! RFC 9457 `application/problem+json` responses.
//!
//! The standard members carry the human-readable parts. A stable machine
//! code travels in the `code` extension member, because a status alone is too
//! coarse for a client to branch on and prose is not something to parse.
//!
//! Nothing here reveals whether a resource exists in another project.
//! `NotFound` is deliberately the same response for "no such sandbox" and
//! "someone else's sandbox", per `docs/api-contract.md`.

use axum::Json;
use axum::response::{IntoResponse, Response};
use http::{StatusCode, header};
use serde::Serialize;

/// A problem the caller can act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Problem {
    /// The request could not be understood.
    BadRequest(&'static str),
    /// No usable credential was presented.
    Unauthenticated,
    /// A valid credential without the required access.
    Forbidden,
    /// Missing, or belonging to another project. Indistinguishable on purpose.
    NotFound,
    /// A required backend is unavailable.
    Unavailable,
    /// Something failed that the caller cannot fix.
    Internal,
}

impl Problem {
    /// The HTTP status this problem maps to.
    #[must_use]
    pub fn status(self) -> StatusCode {
        match self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Unauthenticated => StatusCode::UNAUTHORIZED,
            Self::Forbidden => StatusCode::FORBIDDEN,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// The stable machine-readable code clients branch on.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::BadRequest(_) => "bad_request",
            Self::Unauthenticated => "unauthenticated",
            Self::Forbidden => "forbidden",
            Self::NotFound => "not_found",
            Self::Unavailable => "unavailable",
            Self::Internal => "internal",
        }
    }

    /// A short title. Never carries caller-supplied text or internal detail.
    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            Self::BadRequest(detail) => detail,
            Self::Unauthenticated => "Authentication is required",
            Self::Forbidden => "This credential does not have the required access",
            Self::NotFound => "No such resource",
            Self::Unavailable => "The service is temporarily unable to handle this request",
            Self::Internal => "The request could not be completed",
        }
    }
}

/// The wire shape. `type` is omitted until the documentation URLs exist;
/// RFC 9457 makes it optional and defaulting it to `about:blank` is honest.
#[derive(Debug, Serialize)]
struct Body {
    title: &'static str,
    status: u16,
    code: &'static str,
}

impl IntoResponse for Problem {
    fn into_response(self) -> Response {
        let status = self.status();
        let body = Json(Body {
            title: self.title(),
            status: status.as_u16(),
            code: self.code(),
        });

        let mut response = (status, body).into_response();
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("application/problem+json"),
        );
        // Authentication outcomes must not be cached by anything in between.
        response.headers_mut().insert(
            header::CACHE_CONTROL,
            header::HeaderValue::from_static("no-store"),
        );
        response
    }
}
