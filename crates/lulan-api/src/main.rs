use anyhow::Context;
use lulan_api::config::Config;
use lulan_api::state::AppState;
use lulan_api::{MIGRATOR, router, seed};
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

    // Background machinery (needs a database): outbox relay delivering
    // events to the sink, and the sweeper expiring unpaid orders.
    if let Some(pool) = &db {
        tokio::spawn(lulan_engine::events::run_relay(
            pool.clone(),
            lulan_engine::events::TracingSink,
            std::time::Duration::from_secs(2),
        ));
        let sweeper_store = lulan_engine::orders::OrderStore::new(pool.clone());
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

    let quote_secret = std::sync::Arc::new(match &config.quote_secret {
        Some(secret) => secret.as_bytes().to_vec(),
        None => {
            tracing::warn!("LULAN_QUOTE_SECRET not set — quotes won't survive restarts");
            lulan_api::state::ephemeral_secret()
        }
    });

    let ticket_signer = match &db {
        Some(pool) => Some(std::sync::Arc::new(
            lulan_engine::ticket::TicketSigner::load_or_create(pool).await?,
        )),
        None => None,
    };

    let app = router(AppState {
        db,
        redis,
        pricing,
        quote_secret,
        ticket_signer,
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
