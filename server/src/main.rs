use receipt_roots_server::{
    adapters::{ManualInput, MockPayPal, PayPalPayouts, ReceiptExtractor},
    config::Config,
    domain::Command,
    policy::router_with_proxy,
    sandbox::SandboxPayPal,
    storage::{PostgresStore, SqliteStore, Storage},
    Service,
};
use std::sync::Arc;

#[tokio::main]
async fn main() {
    // Suppress panic payloads/backtraces and startup error internals in deployed binaries.
    std::panic::set_hook(Box::new(|_| eprintln!("server task failed")));
    if run().await.is_err() {
        eprintln!("server startup or runtime failed; check deployment configuration");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_env()?;
    let storage: Arc<dyn Storage> = if config.database_url.starts_with("sqlite:") {
        Arc::new(SqliteStore::connect(&config.database_url).await?)
    } else {
        Arc::new(PostgresStore::connect(&config.database_url).await?)
    };
    storage
        .execute(Command::ResetDemo {
            now: chrono::Utc::now().timestamp(),
        })
        .await?;
    storage
        .execute(Command::ConfigureSandbox(config.paypal_mode == "sandbox"))
        .await?;
    let reset_storage = storage.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
        loop {
            interval.tick().await;
            // No provider calls and no private error details in logs.
            if reset_storage
                .execute(Command::ResetDemo {
                    now: chrono::Utc::now().timestamp(),
                })
                .await
                .is_err()
            {
                eprintln!("daily demo maintenance unavailable");
            }
        }
    });
    storage
        .execute(Command::ConfigureRecipient(config.recipient.clone()))
        .await?;
    let paypal: Arc<dyn PayPalPayouts> = if config.paypal_mode == "sandbox" {
        Arc::new(SandboxPayPal::new(
            config.client_id.clone(),
            config.client_secret.clone(),
        )?)
    } else {
        Arc::new(MockPayPal::new(config.fixture))
    };
    let inbox = if config.paypal_mode == "sandbox" {
        Some(receipt_roots_server::webhook::Inbox {
            storage: storage.clone(),
            paypal: paypal.clone(),
            verifier: Arc::new(receipt_roots_server::webhook::SandboxVerifier::new(
                config.client_id.clone(),
                config.client_secret.clone(),
                config.webhook_id.clone(),
            )?),
        })
    } else {
        None
    };
    let extractor: Arc<dyn ReceiptExtractor> = if config.llm_mode == "gemini-free" {
        Arc::new(receipt_roots_server::extract::GeminiFree::new(
            config.gemini_api_key,
        )?)
    } else {
        Arc::new(ManualInput)
    };
    let service = Arc::new(Service {
        storage,
        paypal,
        extractor,
        finance_token: config.finance_token,
    });
    let listener = tokio::net::TcpListener::bind(config.bind).await?;
    eprintln!(
        "Pawpaid listening on {}; judge access enabled, production disabled",
        listener.local_addr()?
    );
    let mut app = router_with_proxy(
        service,
        config.claimant_password,
        config.finance_password,
        config.trust_loopback_proxy,
    );
    if let Some(inbox) = inbox {
        let worker = inbox.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
            loop {
                interval.tick().await;
                if worker.retry().await.is_err() {
                    eprintln!("sandbox inbox retry failed; reconciliation required");
                }
            }
        });
        app = app.merge(receipt_roots_server::webhook::router_with_proxy(
            inbox,
            config.trust_loopback_proxy,
        ));
    }
    axum::serve(
        listener,
        receipt_roots_server::security::harden_for_mode(app, config.development)
            .into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await?;
    Ok(())
}
