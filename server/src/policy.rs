//! Runtime judge access boundary; test-only core routers do not install this layer.
use crate::{
    http::{claimant_token, HttpError},
    Service,
};
use axum::{
    extract::{ConnectInfo, Request, State},
    http::{HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use contracts::{ApiError, ErrorCode};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
};
use subtle::ConstantTimeEq;
use uuid::Uuid;

// 2026-12-16 00:00 Asia/Taipei, exclusive.
pub const ACCESS_END: i64 = 1_797_350_400;
struct Session {
    finance: bool,
    expires: i64,
}
pub struct AccessPolicy {
    claimant_username: String,
    finance_username: String,
    claimant_password: String,
    finance_password: String,
    sessions: Mutex<BTreeMap<String, Session>>,
    requests: Mutex<BTreeMap<IpAddr, (i64, u32)>>,
    service: Arc<Service>,
    trust_loopback_proxy: bool,
}
impl AccessPolicy {
    pub fn new(service: Arc<Service>, claimant_password: String, finance_password: String) -> Self {
        Self {
            claimant_username: "demo-claimant".into(),
            finance_username: "demo-finance".into(),
            claimant_password,
            finance_password,
            sessions: Mutex::new(BTreeMap::new()),
            requests: Mutex::new(BTreeMap::new()),
            service,
            trust_loopback_proxy: false,
        }
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Login {
    username: String,
    password: String,
}
async fn login(State(policy): State<Arc<AccessPolicy>>, Json(input): Json<Login>) -> Response {
    let finance = input.username == policy.finance_username;
    let expected = if finance {
        &policy.finance_password
    } else {
        &policy.claimant_password
    };
    if (input.username != policy.claimant_username && !finance)
        || !bool::from(input.password.as_bytes().ct_eq(expected.as_bytes()))
    {
        return (StatusCode::UNAUTHORIZED, "invalid judge credentials").into_response();
    }
    let token = format!("{}{}", Uuid::new_v4(), Uuid::new_v4());
    let expires = chrono::Utc::now().timestamp() + 86400;
    policy
        .sessions
        .lock()
        .unwrap()
        .insert(token.clone(), Session { finance, expires });
    Json(serde_json::json!({"token": token, "role": if finance {"finance"} else {"claimant"}, "expires_at": expires})).into_response()
}
pub fn router(
    service: Arc<Service>,
    claimant_password: String,
    finance_password: String,
) -> Router {
    router_with_proxy(service, claimant_password, finance_password, false)
}
pub fn router_with_proxy(
    service: Arc<Service>,
    claimant_password: String,
    finance_password: String,
    trust_loopback_proxy: bool,
) -> Router {
    let mut policy = AccessPolicy::new(service.clone(), claimant_password, finance_password);
    policy.claimant_username =
        std::env::var("JUDGE_CLAIMANT_USERNAME").unwrap_or(policy.claimant_username);
    policy.finance_username =
        std::env::var("JUDGE_FINANCE_USERNAME").unwrap_or(policy.finance_username);
    assert!(
        !policy.claimant_username.trim().is_empty()
            && !policy.finance_username.trim().is_empty()
            && policy.claimant_username != policy.finance_username,
        "distinct nonempty account names are required"
    );
    policy.trust_loopback_proxy = trust_loopback_proxy;
    let policy = Arc::new(policy);
    crate::http::router(service)
        .merge(
            Router::new()
                .route("/api/session", post(login))
                .with_state(policy.clone()),
        )
        .layer(axum::middleware::from_fn_with_state(policy, boundary))
        .route("/health", axum::routing::get(|| async { "ok" }))
}
async fn boundary(
    State(policy): State<Arc<AccessPolicy>>,
    mut request: Request,
    next: Next,
) -> Response {
    let now = chrono::Utc::now().timestamp();
    let reject = || {
        HttpError(ApiError::new(
            ErrorCode::Forbidden,
            "judge sign-in required",
        ))
        .into_response()
    };
    if now >= ACCESS_END {
        return reject();
    }
    // Forwarded address is trusted only for an explicitly configured loopback proxy.
    let peer_ip = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|peer| peer.0.ip())
        .unwrap_or(IpAddr::from([0, 0, 0, 0]));
    let ip = if policy.trust_loopback_proxy && peer_ip.is_loopback() {
        request
            .headers()
            .get("x-real-ip")
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.parse().ok())
            .unwrap_or(peer_ip)
    } else {
        peer_ip
    };
    {
        let mut counters = policy.requests.lock().unwrap();
        counters.retain(|_, (minute, _)| *minute == now / 60);
        let entry = counters.entry(client_network(ip)).or_insert((now / 60, 0));
        if entry.1 >= 30 {
            return (StatusCode::TOO_MANY_REQUESTS, "request limit reached").into_response();
        }
        entry.1 += 1;
    }
    if request.uri().path() == "/api/session" {
        return next.run(request).await;
    }
    let token = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default();
    let finance = {
        let mut sessions = policy.sessions.lock().unwrap();
        sessions.retain(|_, session| session.expires > now);
        match sessions.get(token) {
            Some(session) => session.finance,
            None => return reject(),
        }
    };
    let path = request.uri().path();
    if finance
        && request.method() == axum::http::Method::POST
        && (path == "/api/claims" || path.ends_with("/extract") || path.ends_with("/submit"))
    {
        return reject();
    }
    // Run maintenance before handlers as well as on the idle timer, so the first
    // request after 04:00 cannot act on yesterday's unsent demo approvals.
    if let Err(error) = policy
        .service
        .storage
        .execute(crate::domain::Command::ResetDemo { now })
        .await
    {
        return HttpError(error).into_response();
    }
    let capability = if finance {
        policy.service.finance_token.clone()
    } else {
        claimant_token(&policy.service.finance_token, crate::domain::CLAIMANT_ID)
    };
    request.headers_mut().insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {capability}")).unwrap(),
    );
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        adapters::{MockLlm, MockPayPal, MockPayoutFixture},
        storage::SqliteStore,
    };
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;
    #[tokio::test]
    async fn judge_sessions_expire_and_legacy_capabilities_are_rejected() {
        let paypal = Arc::new(MockPayPal::new(MockPayoutFixture::Success));
        let service = Arc::new(Service {
            storage: Arc::new(SqliteStore::connect("sqlite::memory:").await.unwrap()),
            paypal: paypal.clone(),
            extractor: Arc::new(MockLlm),
            finance_token: "internal-only".into(),
        });
        let policy = Arc::new(AccessPolicy::new(
            service,
            "claimant-password".into(),
            "finance-password".into(),
        ));
        policy.sessions.lock().unwrap().insert(
            "expired-session".into(),
            Session {
                finance: true,
                expires: chrono::Utc::now().timestamp() - 1,
            },
        );
        let app = Router::new()
            .route("/api/session", post(login))
            .route("/api/protected", axum::routing::get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(
                policy.clone(),
                boundary,
            ))
            .with_state(policy.clone());
        for token in ["", "internal-only", "expired-session"] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/api/protected")
                        .header("authorization", format!("Bearer {token}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
        }
        for username in ["demo-claimant", "demo-finance"] {
            let password = if username == "demo-finance" {
                "finance-password"
            } else {
                "claimant-password"
            };
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/session")
                        .header("content-type", "application/json")
                        .body(Body::from(
                            serde_json::json!({"username":username,"password":password})
                                .to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap();
            let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            let token = value["token"].as_str().unwrap();
            let sessions = policy.sessions.lock().unwrap();
            let session = &sessions[token];
            assert_eq!(session.finance, username == "demo-finance");
            assert!((session.expires - chrono::Utc::now().timestamp() - 86400).abs() <= 1);
        }
        assert_eq!(
            paypal.send_calls.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
    }
    #[tokio::test]
    async fn per_peer_rate_limit_includes_login_and_ignores_forwarded_headers() {
        let service = Arc::new(Service {
            storage: Arc::new(SqliteStore::connect("sqlite::memory:").await.unwrap()),
            paypal: Arc::new(MockPayPal::new(MockPayoutFixture::Success)),
            extractor: Arc::new(MockLlm),
            finance_token: "internal".into(),
        });
        let app = router(service, "c".into(), "f".into());
        for n in 0..31 {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/api/config")
                        .header("x-forwarded-for", format!("192.0.2.{n}"))
                        .extension(ConnectInfo("127.0.0.2:1234".parse::<SocketAddr>().unwrap()))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                if n == 30 {
                    StatusCode::TOO_MANY_REQUESTS
                } else {
                    StatusCode::FORBIDDEN
                }
            );
        }
    }
    #[tokio::test]
    async fn login_failures_and_api_share_the_peer_limit() {
        let service = Arc::new(Service {
            storage: Arc::new(SqliteStore::connect("sqlite::memory:").await.unwrap()),
            paypal: Arc::new(MockPayPal::new(MockPayoutFixture::Success)),
            extractor: Arc::new(MockLlm),
            finance_token: "synthetic-internal".into(),
        });
        let app = router(service, "synthetic-c".into(), "synthetic-f".into());
        // Discard a measurement if a minute rollover resets the real counter.
        for attempt in 0..3 {
            let minute = chrono::Utc::now().timestamp() / 60;
            let peer: SocketAddr = format!("192.0.2.{}:1234", attempt + 1).parse().unwrap();
            let mut statuses = Vec::new();
            for n in 0..31 {
                let request = if n < 30 {
                    Request::builder()
                        .method("POST")
                        .uri("/api/session")
                        .header("content-type", "application/json")
                        .extension(ConnectInfo(peer))
                        .body(Body::from(
                            r#"{"username":"demo-finance","password":"synthetic-wrong"}"#,
                        ))
                        .unwrap()
                } else {
                    Request::builder()
                        .uri("/api/config")
                        .extension(ConnectInfo(peer))
                        .body(Body::empty())
                        .unwrap()
                };
                statuses.push(app.clone().oneshot(request).await.unwrap().status());
            }
            if chrono::Utc::now().timestamp() / 60 != minute {
                continue;
            }
            assert!(statuses[..30]
                .iter()
                .all(|s| *s == StatusCode::UNAUTHORIZED));
            assert_eq!(statuses[30], StatusCode::TOO_MANY_REQUESTS);
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/session")
                        .extension(ConnectInfo(peer))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            if chrono::Utc::now().timestamp() / 60 != minute {
                continue;
            }
            assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
            return;
        }
        panic!("inconclusive: repeated minute rollover");
    }
    #[test]
    fn judging_cutoff_is_taipei_midnight() {
        assert_eq!(
            chrono::DateTime::from_timestamp(ACCESS_END, 0)
                .unwrap()
                .to_rfc3339(),
            "2026-12-15T16:00:00+00:00"
        );
    }
}

/// Normalize privacy addresses so a client cannot rotate within its IPv6 /64.
pub fn client_network(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(ip) => {
            if let Some(v4) = ip.to_ipv4_mapped() {
                return IpAddr::V4(v4);
            }
            IpAddr::V6(std::net::Ipv6Addr::from(u128::from(ip) & (u128::MAX << 64)))
        }
        ip => ip,
    }
}
// Engineering proposals, not owner-approved policy.
const PEER_PER_MIN: u32 = 20;
const GLOBAL_PER_MIN: u32 = 120;
// Outer ceiling after classification: uncorrelated traffic cannot spend the 60 reserved slots.
const OUTER_PER_MIN: u32 = 600;
const CORRELATED_RESERVE: u32 = 60;

pub struct IntakeLimit {
    #[cfg(test)]
    fixed_time: Option<i64>,
    trust_proxy: bool,
    // Laned: per-peer before classification; the outer ceiling and GLOBAL_PER_MIN are charged by the
    // handler only to deliveries that name no known payout. Unlaned keeps the single pool.
    laned: bool,
    // (minute, [outer admitted, global uncorrelated, outer uncorrelated], per network)
    counters: Mutex<(i64, [u32; 3], BTreeMap<IpAddr, u32>)>,
}
impl IntakeLimit {
    pub fn new(trust_proxy: bool) -> Self {
        Self {
            #[cfg(test)]
            fixed_time: None,
            trust_proxy,
            laned: false,
            counters: Mutex::new((0, [0; 3], BTreeMap::new())),
        }
    }
    pub fn new_laned(trust_proxy: bool) -> Self {
        Self {
            laned: true,
            ..Self::new(trust_proxy)
        }
    }
    #[cfg(test)]
    pub(crate) fn at_time(trust_proxy: bool, now: i64) -> Self {
        Self {
            fixed_time: Some(now),
            ..Self::new(trust_proxy)
        }
    }
    #[cfg(test)]
    pub(crate) fn at_time_laned(trust_proxy: bool, now: i64) -> Self {
        Self {
            fixed_time: Some(now),
            ..Self::new_laned(trust_proxy)
        }
    }
    pub fn is_laned(&self) -> bool {
        self.laned
    }
    /// Charges the uncorrelated global window. Always true when not laned (already charged).
    pub fn admit_uncorrelated(&self) -> bool {
        self.admit_uncorrelated_at(self.now())
    }
    fn admit_uncorrelated_at(&self, now: i64) -> bool {
        if !self.laned {
            return true;
        }
        let mut c = self.counters.lock().unwrap();
        if c.0 != now / 60 {
            *c = (now / 60, [0; 3], BTreeMap::new());
        }
        if c.1[1] >= GLOBAL_PER_MIN {
            return false;
        }
        c.1[1] += 1;
        true
    }
    /// Charge after classification; uncorrelated requests cannot spend the 60 reserved slots.
    pub fn admit_outer(&self, correlated: bool) -> bool {
        if !self.laned {
            return true;
        }
        let now = self.now();
        let mut c = self.counters.lock().unwrap();
        if c.0 != now / 60 {
            *c = (now / 60, [0; 3], BTreeMap::new());
        }
        if c.1[0] >= OUTER_PER_MIN || (!correlated && c.1[2] >= OUTER_PER_MIN - CORRELATED_RESERVE)
        {
            return false;
        }
        c.1[0] += 1;
        if !correlated {
            c.1[2] += 1;
        }
        true
    }
    fn now(&self) -> i64 {
        #[cfg(test)]
        if let Some(now) = self.fixed_time {
            return now;
        }
        chrono::Utc::now().timestamp()
    }
    fn admit(&self, ip: IpAddr, now: i64) -> bool {
        let mut c = self.counters.lock().unwrap();
        if c.0 != now / 60 {
            *c = (now / 60, [0; 3], BTreeMap::new());
        }
        if (!self.laned && c.1[0] >= GLOBAL_PER_MIN)
            || c.2.get(&client_network(ip)).copied().unwrap_or(0) >= PEER_PER_MIN
        {
            return false;
        }
        if !self.laned {
            c.1[0] += 1;
        }
        *c.2.entry(client_network(ip)).or_default() += 1;
        true
    }
}
pub async fn limit_webhook(
    State(limit): State<Arc<IntakeLimit>>,
    request: Request,
    next: Next,
) -> Response {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|p| p.0.ip())
        .unwrap_or(IpAddr::from([0, 0, 0, 0]));
    let ip = if limit.trust_proxy && peer.is_loopback() {
        request
            .headers()
            .get("x-real-ip")
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.parse().ok())
            .unwrap_or(peer)
    } else {
        peer
    };
    if !limit.admit(ip, limit.now()) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            "webhook request limit reached",
        )
            .into_response();
    }
    next.run(request).await
}

#[cfg(test)]
mod intake_tests {
    use super::*;
    #[test]
    fn ipv6_rotation_shares_limit_and_global_limit_bounds_distinct_peers() {
        let limit = IntakeLimit::new(false);
        for n in 0..20 {
            assert!(limit.admit(format!("2001:db8::{}", n + 1).parse().unwrap(), 60));
        }
        assert!(!limit.admit("2001:db8::ffff".parse().unwrap(), 60));
        for n in 0..100 {
            assert!(limit.admit(IpAddr::from([192, 0, 2, n]), 60));
        }
        assert!(!limit.admit("198.51.100.1".parse().unwrap(), 60));
        assert!(limit.admit("2001:db8::ffff".parse().unwrap(), 120));
    }
    // The peer ceiling precedes classification; the outer ceiling reserves correlated capacity.
    #[test]
    fn laned_global_window_charges_only_uncorrelated_and_outer_ceiling_holds() {
        let limit = IntakeLimit::at_time_laned(false, 60);
        for n in 0..120u32 {
            let ip = IpAddr::from([10, 0, (n / 20) as u8, (n % 20) as u8]);
            assert!(limit.admit(ip, 60));
            assert!(limit.admit_uncorrelated_at(60), "uncorrelated {n}");
        }
        // Window spent: uncorrelated is refused, but per-peer admission still passes, which
        // is what lets a correlated delivery reach storage.
        assert!(!limit.admit_uncorrelated_at(60));
        assert!(limit.admit(IpAddr::from([10, 1, 0, 1]), 60));
        // Per-peer still applies to every lane.
        for _ in 0..19 {
            assert!(limit.admit(IpAddr::from([10, 1, 0, 1]), 60));
        }
        assert!(!limit.admit(IpAddr::from([10, 1, 0, 1]), 60));
        for _ in 0..540 {
            assert!(limit.admit_outer(false));
        }
        assert!(!limit.admit_outer(false));
        // Classification is still reachable after the uncorrelated outer lane is spent.
        assert!(limit.admit(IpAddr::from([172, 31, 255, 1]), 60));
        for _ in 0..60 {
            assert!(limit.admit_outer(true));
        }
        assert!(!limit.admit_outer(true));
        // A new minute resets both counters.
        assert!(limit.admit(IpAddr::from([10, 1, 0, 1]), 120));
        assert!(limit.admit_uncorrelated_at(120));
    }
    #[test]
    fn unlaned_limit_keeps_the_single_pool_and_never_double_charges() {
        let limit = IntakeLimit::at_time(false, 60);
        assert!(!limit.is_laned());
        assert!(limit.admit_uncorrelated_at(60));
    }
}
