use axum::{
    Json,
    extract::State,
    http::{StatusCode, header},
};
use serde::Serialize;

use crate::app::AppState;

/// The landing response. It carries the deployment's identity because that
/// is the one thing an operator cannot get any other way: `/` is the URL a
/// person already has in hand, and asking it "what are you running?" should
/// not require knowing that a second route exists.
#[derive(Serialize)]
pub struct ServiceInfo {
    name: &'static str,
    status: &'static str,
    version: &'static str,
    commit: &'static str,
}

#[derive(Serialize)]
pub struct VersionInfo {
    name: &'static str,
    version: &'static str,
    commit: &'static str,
}

#[derive(Serialize)]
pub struct HealthInfo {
    service: &'static str,
    runtime: &'static str,
    status: &'static str,
}

pub async fn index() -> Json<ServiceInfo> {
    Json(ServiceInfo {
        name: env!("CARGO_PKG_NAME"),
        status: "ok",
        version: env!("VELA_RELAY_RELEASE"),
        commit: env!("VELA_RELAY_BUILD_SHA"),
    })
}

pub async fn liveness() -> StatusCode {
    StatusCode::NO_CONTENT
}

pub async fn health() -> ([(header::HeaderName, &'static str); 1], Json<HealthInfo>) {
    (
        [(header::CACHE_CONTROL, "no-cache, no-store, must-revalidate")],
        Json(HealthInfo {
            service: "vela-relay",
            runtime: "tokio",
            status: "ok",
        }),
    )
}

pub async fn readiness(State(state): State<AppState>) -> StatusCode {
    if state.readiness().is_ready() {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

pub async fn version() -> Json<VersionInfo> {
    Json(VersionInfo {
        name: env!("CARGO_PKG_NAME"),
        // The release tag, not `CARGO_PKG_VERSION` — that has read 0.1.0
        // across every release this repository has tagged.
        version: env!("VELA_RELAY_RELEASE"),
        commit: env!("VELA_RELAY_BUILD_SHA"),
    })
}
