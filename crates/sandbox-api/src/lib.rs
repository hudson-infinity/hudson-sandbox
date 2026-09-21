//! The HTTP API: authentication, admission, status, and the OpenAPI surface.
//!
//! Routes are added as the operations behind them exist. A route that returns
//! a plausible shape without doing the work is worse than no route, because a
//! client cannot tell the difference.

pub mod auth;
pub mod problem;

use axum::Router;
use axum::extract::FromRef;
use sandbox_store::Store;

/// Everything a handler needs.
#[derive(Debug, Clone)]
pub struct AppState {
    /// The durable store.
    pub store: Store,
}

impl FromRef<AppState> for Store {
    fn from_ref(state: &AppState) -> Self {
        state.store.clone()
    }
}

/// Build the router.
///
/// Empty of routes so far by design: authentication exists, the operations it
/// guards do not.
pub fn router(state: AppState) -> Router {
    Router::new().with_state(state)
}
