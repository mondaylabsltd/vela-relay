use axum::{
    Router,
    http::StatusCode,
    routing::{get, post},
};
use tower::{ServiceBuilder, limit::ConcurrencyLimitLayer};
use tower_http::{
    catch_panic::CatchPanicLayer,
    cors::CorsLayer,
    limit::RequestBodyLimitLayer,
    timeout::TimeoutLayer,
    trace::{DefaultMakeSpan, DefaultOnResponse, TraceLayer},
};

mod handlers;
mod queue;
pub(crate) mod rpc;
pub mod state;
mod user_operation_store;

pub(crate) use queue::USER_OPERATION_QUEUE_RETENTION;
pub use queue::UserOperationQueue;
pub use state::{AppState, Readiness};
pub use user_operation_store::{
    ClaimedDelayedUserOperation, DelayedUserOperation, PreparedBundleIntent,
    PreparedSimulationDeploymentIntent, StoredUserOperation, UserOperationStatusStore,
};

use crate::utils::config::HttpConfig;
use handlers::system;

pub fn router(config: &HttpConfig, state: AppState) -> Router {
    Router::new()
        .route("/", get(system::index))
        .route("/health", get(system::health))
        .route("/api/health", get(system::health))
        .route("/healthz", get(system::liveness))
        .route("/readyz", get(system::readiness))
        .route("/version", get(system::version))
        .route(
            "/v1/account/{chain_id}/{safe_address}",
            get(handlers::account::handle),
        )
        .route("/v1/treasury", get(handlers::treasury::address))
        .route("/v1/treasury/{chain_id}", get(handlers::treasury::status))
        .route("/{chain_id}", post(rpc::handle))
        .layer(
            ServiceBuilder::new()
                .layer(CorsLayer::permissive())
                .layer(CatchPanicLayer::new())
                .layer(
                    TraceLayer::new_for_http()
                        .make_span_with(DefaultMakeSpan::new().level(tracing::Level::INFO))
                        .on_response(DefaultOnResponse::new().level(tracing::Level::INFO)),
                )
                .layer(TimeoutLayer::with_status_code(
                    StatusCode::REQUEST_TIMEOUT,
                    config.request_timeout,
                ))
                .layer(ConcurrencyLimitLayer::new(config.max_concurrency))
                .layer(RequestBodyLimitLayer::new(config.max_body_bytes)),
        )
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::{
        body::Body,
        http::{Request, StatusCode, header},
    };
    use tower::ServiceExt;

    use super::router;
    use crate::{app::AppState, utils::config::HttpConfig};

    fn http_config() -> HttpConfig {
        HttpConfig {
            request_timeout: Duration::from_secs(30),
            max_body_bytes: 1_024,
            max_concurrency: 16,
        }
    }

    #[tokio::test]
    async fn readiness_requires_every_worker_job() {
        let state = AppState::with_settlement_recipient(&["cleanup", "parallel"], None);

        let response = router(&http_config(), state.clone())
            .oneshot(Request::get("/readyz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

        state.readiness().mark_job_ready("cleanup");
        state.readiness().mark_job_ready("parallel");

        let response = router(&http_config(), state)
            .oneshot(Request::get("/readyz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn reports_global_health_without_caching() {
        let response = router(
            &http_config(),
            AppState::with_settlement_recipient(&[], None),
        )
        .oneshot(
            Request::get("/api/health?_t=1784439163501")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "no-cache, no-store, must-revalidate"
        );

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            serde_json::json!({
                "service": "vela-relay",
                "runtime": "tokio",
                "status": "ok",
            })
        );
    }

    #[tokio::test]
    async fn rejects_an_invalid_safe_address_for_the_account_endpoint() {
        let response = router(
            &http_config(),
            AppState::with_settlement_recipient(&[], None),
        )
        .oneshot(
            Request::get("/v1/account/42161/not-an-address")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            serde_json::json!({ "error": "invalid safeAddress" })
        );
    }

    #[tokio::test]
    async fn returns_the_configured_treasury_address() {
        let response = router(
            &http_config(),
            AppState::with_settlement_recipient(
                &[],
                Some("0xee2cca98ecbff34663591a925968fa4db5a1f0dd".into()),
            ),
        )
        .oneshot(Request::get("/v1/treasury").body(Body::empty()).unwrap())
        .await
        .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            serde_json::json!({
                "address": "0xee2cca98ecbff34663591a925968fa4db5a1f0dd",
            })
        );
    }

    #[tokio::test]
    async fn rejects_requests_with_oversized_content_length() {
        let state = AppState::with_settlement_recipient(&[], None);
        let response = router(&http_config(), state)
            .oneshot(
                Request::get("/")
                    .header("content-length", "1025")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn the_landing_and_version_routes_name_the_running_build() {
        // What an operator reads off a deployment to know what is on it. The
        // point is that both fields are BAKED IN at compile time: a response
        // cannot claim a release the binary is not, and cannot go stale while
        // the process keeps running.
        for path in ["/", "/version"] {
            let response = router(
                &http_config(),
                AppState::with_settlement_recipient(&[], None),
            )
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();

            assert_eq!(response.status(), StatusCode::OK);
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();

            assert_eq!(body["name"], "vela-relay");
            assert_eq!(body["version"], env!("VELA_RELAY_RELEASE"));
            assert_eq!(body["commit"], env!("VELA_RELAY_BUILD_SHA"));
            // The version reported must be the RELEASE, never the package
            // version: that has read 0.1.0 across every tag this repository
            // has cut, so reporting it would answer the operator's question
            // with a number that cannot change.
            assert_ne!(body["version"], env!("CARGO_PKG_VERSION"));
            // And neither field may be a placeholder standing in for a real
            // build — the failure mode that made the Worker report "dev" on
            // every live deployment.
            for field in ["version", "commit"] {
                let value = body[field].as_str().expect(field);
                assert!(!value.is_empty(), "{path} {field}");
                assert_ne!(value, "unknown", "{path} {field}");
                assert_ne!(value, "dev", "{path} {field}");
            }
        }
    }

    #[tokio::test]
    async fn permits_cross_origin_requests() {
        let state = AppState::with_settlement_recipient(&[], None);
        let response = router(&http_config(), state)
            .oneshot(
                Request::get("/")
                    .header(header::ORIGIN, "https://app.example.com")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN], "*");
    }
}
