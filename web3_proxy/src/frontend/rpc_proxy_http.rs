//! Take a user's HTTP JSON-RPC requests and either respond from local data or proxy the request to a backend rpc server.

use super::authorization::{ip_is_authorized, key_is_authorized};
use super::errors::FrontendResult;
use super::rpc_proxy_ws::ProxyMode;
use crate::{app::Web3ProxyApp, jsonrpc::JsonRpcRequestEnum};
use axum::extract::Path;
use axum::headers::{Origin, Referer, UserAgent};
use axum::TypedHeader;
use axum::{response::IntoResponse, Extension, Json};
use axum_client_ip::InsecureClientIp;
use axum_macros::debug_handler;
use itertools::Itertools;
use std::sync::Arc;

/// POST /rpc -- Public entrypoint for HTTP JSON-RPC requests. Web3 wallets use this.
/// Defaults to rate limiting by IP address, but can also read the Authorization header for a bearer token.
/// If possible, please use a WebSocket instead.
#[debug_handler]
pub async fn proxy_web3_rpc(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    ip: InsecureClientIp,
    origin: Option<TypedHeader<Origin>>,
    Json(payload): Json<JsonRpcRequestEnum>,
) -> FrontendResult {
    _proxy_web3_rpc(app, ip, origin, payload, ProxyMode::Best).await
}

#[debug_handler]
pub async fn fastest_proxy_web3_rpc(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    ip: InsecureClientIp,
    origin: Option<TypedHeader<Origin>>,
    Json(payload): Json<JsonRpcRequestEnum>,
) -> FrontendResult {
    // TODO: read the fastest number from params
    // TODO: check that the app allows this without authentication
    _proxy_web3_rpc(app, ip, origin, payload, ProxyMode::Fastest(0)).await
}

#[debug_handler]
pub async fn versus_proxy_web3_rpc(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    ip: InsecureClientIp,
    origin: Option<TypedHeader<Origin>>,
    Json(payload): Json<JsonRpcRequestEnum>,
) -> FrontendResult {
    _proxy_web3_rpc(app, ip, origin, payload, ProxyMode::Versus).await
}

async fn _proxy_web3_rpc(
    app: Arc<Web3ProxyApp>,
    InsecureClientIp(ip): InsecureClientIp,
    origin: Option<TypedHeader<Origin>>,
    payload: JsonRpcRequestEnum,
    proxy_mode: ProxyMode,
) -> FrontendResult {
    // TODO: benchmark spawning this
    // TODO: do we care about keeping the TypedHeader wrapper?
    let origin = origin.map(|x| x.0);

    let (authorization, semaphore) = ip_is_authorized(&app, ip, origin, proxy_mode).await?;

    let authorization = Arc::new(authorization);

    let (response, rpcs, _semaphore) = app
        .proxy_web3_rpc(authorization, payload)
        .await
        .map(|(x, y)| (x, y, semaphore))?;

    let mut response = Json(&response).into_response();

    let headers = response.headers_mut();

    // TODO: this might be slow. think about this more
    // TODO: special string if no rpcs were used (cache hit)?
    let mut backup_used = false;

    let rpcs: String = rpcs
        .into_iter()
        .map(|x| {
            if x.backup {
                backup_used = true;
            }
            x.name.clone()
        })
        .join(",");

    headers.insert(
        "X-W3P-BACKEND-RPCS",
        rpcs.parse().expect("W3P-BACKEND-RPCS should always parse"),
    );

    headers.insert(
        "X-W3P-BACKUP-RPC",
        backup_used
            .to_string()
            .parse()
            .expect("W3P-BACKEND-RPCS should always parse"),
    );

    // TODO: add a header if a backend rpc was used

    headers.insert(
        "X-W3P-CLIENT-IP",
        ip.to_string()
            .parse()
            .expect("X-CLIENT-IP should always parse"),
    );

    Ok(response)
}

/// Authenticated entrypoint for HTTP JSON-RPC requests. Web3 wallets use this.
/// Rate limit and billing based on the api key in the url.
/// Can optionally authorized based on origin, referer, or user agent.
/// If possible, please use a WebSocket instead.
#[debug_handler]
pub async fn proxy_web3_rpc_with_key(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    ip: InsecureClientIp,
    origin: Option<TypedHeader<Origin>>,
    referer: Option<TypedHeader<Referer>>,
    user_agent: Option<TypedHeader<UserAgent>>,
    Path(rpc_key): Path<String>,
    Json(payload): Json<JsonRpcRequestEnum>,
) -> FrontendResult {
    _proxy_web3_rpc_with_key(
        app,
        ip,
        origin,
        referer,
        user_agent,
        rpc_key,
        payload,
        ProxyMode::Best,
    )
    .await
}

#[debug_handler]
pub async fn debug_proxy_web3_rpc_with_key(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    ip: InsecureClientIp,
    origin: Option<TypedHeader<Origin>>,
    referer: Option<TypedHeader<Referer>>,
    user_agent: Option<TypedHeader<UserAgent>>,
    Path(rpc_key): Path<String>,
    Json(payload): Json<JsonRpcRequestEnum>,
) -> FrontendResult {
    _proxy_web3_rpc_with_key(
        app,
        ip,
        origin,
        referer,
        user_agent,
        rpc_key,
        payload,
        ProxyMode::Debug,
    )
    .await
}

#[debug_handler]
pub async fn fastest_proxy_web3_rpc_with_key(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    ip: InsecureClientIp,
    origin: Option<TypedHeader<Origin>>,
    referer: Option<TypedHeader<Referer>>,
    user_agent: Option<TypedHeader<UserAgent>>,
    Path(rpc_key): Path<String>,
    Json(payload): Json<JsonRpcRequestEnum>,
) -> FrontendResult {
    _proxy_web3_rpc_with_key(
        app,
        ip,
        origin,
        referer,
        user_agent,
        rpc_key,
        payload,
        ProxyMode::Fastest(0),
    )
    .await
}

#[debug_handler]
pub async fn versus_proxy_web3_rpc_with_key(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    ip: InsecureClientIp,
    origin: Option<TypedHeader<Origin>>,
    referer: Option<TypedHeader<Referer>>,
    user_agent: Option<TypedHeader<UserAgent>>,
    Path(rpc_key): Path<String>,
    Json(payload): Json<JsonRpcRequestEnum>,
) -> FrontendResult {
    _proxy_web3_rpc_with_key(
        app,
        ip,
        origin,
        referer,
        user_agent,
        rpc_key,
        payload,
        ProxyMode::Versus,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn _proxy_web3_rpc_with_key(
    app: Arc<Web3ProxyApp>,
    InsecureClientIp(ip): InsecureClientIp,
    origin: Option<TypedHeader<Origin>>,
    referer: Option<TypedHeader<Referer>>,
    user_agent: Option<TypedHeader<UserAgent>>,
    rpc_key: String,
    payload: JsonRpcRequestEnum,
    proxy_mode: ProxyMode,
) -> FrontendResult {
    // TODO: DRY w/ proxy_web3_rpc
    // the request can take a while, so we spawn so that we can start serving another request
    let rpc_key = rpc_key.parse()?;

    let (authorization, semaphore) = key_is_authorized(
        &app,
        rpc_key,
        ip,
        origin.map(|x| x.0),
        proxy_mode,
        referer.map(|x| x.0),
        user_agent.map(|x| x.0),
    )
    .await?;

    let authorization = Arc::new(authorization);

    let rpc_secret_key_id = authorization.checks.rpc_secret_key_id;

    let (response, rpcs, _semaphore) = app
        .proxy_web3_rpc(authorization, payload)
        .await
        .map(|(x, y)| (x, y, semaphore))?;

    let mut response = Json(&response).into_response();

    let headers = response.headers_mut();

    let mut backup_used = false;

    // TODO: special string if no rpcs were used (cache hit)? or is an empty string fine? maybe the rpc name + "cached"
    let rpcs: String = rpcs
        .into_iter()
        .map(|x| {
            if x.backup {
                backup_used = true;
            }
            x.name.clone()
        })
        .join(",");

    headers.insert(
        "X-W3P-BACKEND-RPCs",
        rpcs.parse().expect("W3P-BACKEND-RPCS should always parse"),
    );

    headers.insert(
        "X-W3P-BACKUP-RPC",
        backup_used
            .to_string()
            .parse()
            .expect("W3P-BACKEND-RPCS should always parse"),
    );

    headers.insert(
        "X-W3P-CLIENT-IP",
        ip.to_string()
            .parse()
            .expect("X-CLIENT-IP should always parse"),
    );

    if let Some(rpc_secret_key_id) = rpc_secret_key_id {
        headers.insert(
            "X-W3P-KEY-ID",
            rpc_secret_key_id
                .to_string()
                .parse()
                .expect("X-CLIENT-IP should always parse"),
        );
    }

    Ok(response)
}
