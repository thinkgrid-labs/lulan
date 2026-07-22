use anyhow::Context;
use lulan_api::config::Config;
use lulan_api::state::AppState;
use lulan_api::{MIGRATOR, router, seed};
use lulan_engine::payments::PaymentProvider as _;

/// 32 bytes of hex is the shortest thing worth calling an HMAC key.
const MIN_QUOTE_SECRET_LEN: usize = 32;

/// Per-tick budget for the background cancellation drain. Larger than
/// the inline batch — nobody is waiting on it.
const SWEEP_CASCADE_LIMIT: i64 = 200;
use sqlx::postgres::PgPoolOptions;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "lulan_api=info,lulan_engine=info,tower_http=info".into()),
        )
        .init();

    let config = Config::from_env();

    if std::env::args().nth(1).as_deref() == Some("seed") {
        let url = config
            .database_url
            .context("DATABASE_URL is required for seeding")?;
        let pool = connect(&url).await?;
        seed::seed(&pool).await?;
        return Ok(());
    }

    if std::env::args().nth(1).as_deref() == Some("import-gtfs") {
        let args: Vec<String> = std::env::args().collect();
        let dir = args
            .get(2)
            .filter(|a| !a.starts_with("--"))
            .context("usage: lulan-api import-gtfs <dir> [--days N] [--seats N] [--vessel CODE]")?;
        let mut options = lulan_api::gtfs::GtfsOptions::default();
        let mut i = 3;
        while i < args.len() {
            match args[i].as_str() {
                "--days" => {
                    options.days = args.get(i + 1).context("--days needs a value")?.parse()?;
                    i += 2;
                }
                "--seats" => {
                    options.seats = args.get(i + 1).context("--seats needs a value")?.parse()?;
                    i += 2;
                }
                "--vessel" => {
                    options.vessel =
                        Some(args.get(i + 1).context("--vessel needs a value")?.clone());
                    i += 2;
                }
                other => anyhow::bail!("unknown flag {other}"),
            }
        }
        let url = config
            .database_url
            .context("DATABASE_URL is required for import")?;
        let pool = connect(&url).await?;
        lulan_api::gtfs::import(&pool, std::path::Path::new(dir), options).await?;
        return Ok(());
    }

    let db = match &config.database_url {
        Some(url) => Some(connect(url).await?),
        None => {
            tracing::warn!("DATABASE_URL not set — booting without a database");
            None
        }
    };

    // Redis is optional and non-fatal: holds degrade to 503, claims are
    // unaffected (ADR 0002).
    let redis = match &config.redis_url {
        Some(url) => match connect_redis(url).await {
            Ok(conn) => {
                tracing::info!("redis connected, holds enabled");
                Some(conn)
            }
            Err(err) => {
                tracing::warn!(error = %err, "redis unavailable — holds disabled");
                None
            }
        },
        None => {
            tracing::warn!("REDIS_URL not set — holds disabled");
            None
        }
    };

    // Payment provider: a preset name, a path to a provider description,
    // or nothing. "Nothing" is loud on purpose — the fake provider issues
    // tickets without taking money, which is a demo, not a deployment.
    let payments: std::sync::Arc<dyn lulan_engine::payments::PaymentProvider> = match &config
        .payment_provider
    {
        Some(spec) => {
            let source = match lulan_engine::payments::http::preset::by_name(spec) {
                Some(preset) => preset.to_string(),
                None => std::fs::read_to_string(spec).with_context(|| {
                    format!(
                        "LULAN_PAYMENT_PROVIDER={spec:?} is neither a built-in preset ({}) \
                             nor a readable provider description",
                        lulan_engine::payments::http::preset::NAMES.join(", ")
                    )
                })?,
            };
            let provider_config = lulan_engine::payments::http::ProviderConfig::from_json(&source)
                .map_err(anyhow::Error::msg)?;
            let provider = lulan_engine::payments::http::HttpProvider::new(provider_config)
                .map_err(anyhow::Error::msg)?;
            tracing::info!(
                provider = provider.name(),
                signed_callbacks = provider.authenticates_callbacks(),
                "payments: provider configured"
            );
            std::sync::Arc::new(provider)
        }
        None => {
            tracing::warn!(
                "payments: no provider configured (LULAN_PAYMENT_PROVIDER) — using the fake \
                     provider, which captures payment WITHOUT taking money"
            );
            std::sync::Arc::new(lulan_engine::payments::FakeProvider)
        }
    };

    // This key signs quote tokens AND guest order-retrieval tokens. The
    // second is what makes it fatal rather than a warning: retrieval
    // tokens are long-lived magic links delivered to customers, so an
    // ephemeral key silently breaks every guest's access to their own
    // booking on restart, and breaks it immediately across replicas.
    // Background machinery (needs a database): outbox relay fanning
    // events into the webhook delivery queue, the delivery worker POSTing
    // them, and the sweeper expiring unpaid orders.
    if let Some(pool) = &db {
        if let Ok(key) = std::env::var("LULAN_BOOTSTRAP_ADMIN_KEY") {
            lulan_api::auth::bootstrap_admin_key(pool, &key).await?;
            tracing::info!("bootstrap admin API key active");
        }
        if let Ok(spec) = std::env::var("LULAN_BOOTSTRAP_ADMIN_STAFF") {
            lulan_api::staff::bootstrap_admin_staff(pool, &spec).await?;
            tracing::info!("bootstrap admin staff active");
        }
        tokio::spawn(lulan_engine::events::run_relay(
            pool.clone(),
            lulan_engine::webhooks::WebhookSink::new(pool.clone()),
            std::time::Duration::from_secs(2),
        ));
        tokio::spawn(lulan_engine::webhooks::run_delivery_worker(
            pool.clone(),
            std::time::Duration::from_secs(2),
        ));
        let sweeper_store = lulan_engine::orders::OrderStore::new(pool.clone());
        let sweeper_payments = payments.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(30));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                match sweeper_store.expire_due().await {
                    Ok(0) => {}
                    Ok(n) => tracing::info!(expired = n, "expired overdue orders"),
                    Err(err) => tracing::error!(error = %err, "order expiry sweep failed"),
                }
                // Reclaims keys whose booking died mid-flight, and expires
                // stored responses past any sane retry window.
                match sweeper_store.sweep_idempotency_keys().await {
                    Ok(0) => {}
                    Ok(n) => tracing::debug!(removed = n, "swept idempotency keys"),
                    Err(err) => tracing::error!(error = %err, "idempotency sweep failed"),
                }
                // Drains what a trip cancellation could not settle inline.
                // Driven off trips.status, so it resumes across restarts
                // and retries anything a provider refused.
                match sweeper_store
                    .settle_cancelled_trips(sweeper_payments.as_ref(), SWEEP_CASCADE_LIMIT)
                    .await
                {
                    Ok(stats) if stats.cancelled + stats.refunded + stats.failed > 0 => {
                        tracing::info!(
                            cancelled = stats.cancelled,
                            refunded = stats.refunded,
                            failed = stats.failed,
                            remaining = stats.remaining,
                            "settled orders on cancelled trips"
                        );
                    }
                    Ok(_) => {}
                    Err(err) => tracing::error!(error = %err, "cancellation cascade failed"),
                }
            }
        });
    }

    // Pricing engine: an operator-supplied WASM module (the ADR 0003
    // plugin path — a runtime artifact, not an image layer) or native.
    let pricing: std::sync::Arc<dyn lulan_pricing::PricingEngine> = match &config.pricing_wasm {
        Some(path) => {
            let engine = lulan_pricing::WasmEngine::from_file(std::path::Path::new(path))?;
            tracing::info!(module = %path, "pricing: WASM module loaded");
            std::sync::Arc::new(engine)
        }
        None => {
            tracing::info!("pricing: native rule engine");
            std::sync::Arc::new(lulan_pricing::NativeEngine)
        }
    };

    let quote_secret = std::sync::Arc::new(match (&config.quote_secret, &db) {
        (Some(secret), _) => {
            if secret.len() < MIN_QUOTE_SECRET_LEN {
                anyhow::bail!(
                    "LULAN_QUOTE_SECRET must be at least {MIN_QUOTE_SECRET_LEN} characters \
                     (it signs order-retrieval tokens). Generate one with: \
                     openssl rand -hex 32"
                );
            }
            secret.as_bytes().to_vec()
        }
        (None, Some(_)) => anyhow::bail!(
            "LULAN_QUOTE_SECRET is required. It signs quote tokens and the retrieval \
             tokens guests use to reach their own bookings, so a per-boot value would \
             invalidate every outstanding magic link on restart and would not match \
             across replicas. Generate one with: openssl rand -hex 32"
        ),
        // No database: an infra-less boot (health checks, CI smoke tests)
        // where nothing durable is signed anyway.
        (None, None) => lulan_api::state::ephemeral_secret(),
    });

    let ticket_signer = match &db {
        Some(pool) => Some(std::sync::Arc::new(
            lulan_engine::ticket::TicketSigner::load_or_create(pool).await?,
        )),
        None => None,
    };

    // Customer identity port: HS256 JWT adapter when configured.
    let identity: Option<std::sync::Arc<dyn lulan_api::identity::IdentityProvider>> =
        match lulan_api::identity::HsJwtIdentity::from_env() {
            Some(provider) => {
                tracing::info!("identity: HS256 JWT provider configured");
                Some(std::sync::Arc::new(provider))
            }
            None => {
                tracing::info!("identity: no IdP configured — guest checkout only");
                None
            }
        };

    let app = router(AppState {
        db,
        redis,
        pricing,
        payments,
        quote_secret,
        ticket_signer,
        identity,
    });

    let listener = tokio::net::TcpListener::bind(&config.listen_addr).await?;
    tracing::info!(addr = %config.listen_addr, "lulan-api listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn connect_redis(url: &str) -> anyhow::Result<redis::aio::ConnectionManager> {
    let client = redis::Client::open(url)?;
    Ok(client.get_connection_manager().await?)
}

async fn connect(url: &str) -> anyhow::Result<sqlx::PgPool> {
    let max_connections = std::env::var("LULAN_DB_POOL")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(32);
    let pool = PgPoolOptions::new()
        .max_connections(max_connections)
        .acquire_timeout(std::time::Duration::from_secs(30))
        .connect(url)
        .await?;
    MIGRATOR.run(&pool).await?;
    tracing::info!("database connected, migrations applied");
    Ok(pool)
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.ok();
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutdown signal received");
}
