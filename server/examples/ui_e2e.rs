//! Loopback-only browser test host with the real API, judge policy, Postgres, and test PayPal adapter.
use receipt_roots_server::{
    adapters::{ManualInput, MockPayPal, MockPayoutFixture},
    domain::Command,
    policy,
    storage::{PostgresStore, Storage},
    Service,
};
use std::sync::Arc;

#[tokio::main]
async fn main() {
    let url = std::env::var("TEST_DATABASE_URL").expect("local TEST_DATABASE_URL required");
    let parsed: reqwest::Url = url.parse().unwrap();
    assert_eq!(parsed.host_str(), Some("127.0.0.1"));
    let storage = Arc::new(PostgresStore::connect(&url).await.unwrap());
    storage
        .execute(Command::ConfigureRecipient("clinic@example.invalid".into()))
        .await
        .unwrap();
    storage
        .execute(Command::ConfigureSandbox(true))
        .await
        .unwrap();
    let service = Arc::new(Service {
        storage,
        paypal: Arc::new(MockPayPal::new(MockPayoutFixture::Success)),
        extractor: Arc::new(ManualInput),
        finance_token: "ui-e2e-synthetic-finance".into(),
    });
    let app = policy::router_with_proxy(
        service,
        "ui-e2e-claimant".into(),
        "ui-e2e-finance".into(),
        true,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    println!("UI_E2E_ADDR={}", listener.local_addr().unwrap());
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await
    .unwrap();
}
