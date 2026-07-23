//! Tracing setup, and OTLP span export when built with `--features otlp`.
//!
//! Deferred three times during development for a good reason: the exporter
//! pulls in a large dependency tree, and `/metrics` already answers the
//! operational questions (throughput, latency, error rate). Spans earn
//! their weight only when you need to see *inside* one slow booking —
//! which stage of hold → price → claim → capture actually cost the time.
//!
//! So it is a feature, not a dependency. The default build is unchanged;
//! `cargo build --features otlp` plus `LULAN_OTLP_ENDPOINT` turns it on.

/// Install the subscriber. Returns a guard that must stay alive for the
/// process lifetime — dropping it flushes pending spans.
pub fn init() -> Guard {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "lulan_api=info,lulan_engine=info,tower_http=info".into());

    #[cfg(feature = "otlp")]
    {
        if let Ok(endpoint) = std::env::var("LULAN_OTLP_ENDPOINT") {
            match otlp_layer(&endpoint) {
                Ok(provider) => {
                    use tracing_subscriber::layer::SubscriberExt as _;
                    use tracing_subscriber::util::SubscriberInitExt as _;
                    let tracer =
                        opentelemetry::trace::TracerProvider::tracer(&provider, "lulan-api");
                    tracing_subscriber::registry()
                        .with(filter)
                        .with(tracing_subscriber::fmt::layer())
                        .with(tracing_opentelemetry::layer().with_tracer(tracer))
                        .init();
                    tracing::info!(%endpoint, "telemetry: exporting spans over OTLP");
                    return Guard {
                        provider: Some(provider),
                    };
                }
                Err(err) => {
                    // A telemetry misconfiguration must not stop the
                    // service from selling tickets.
                    eprintln!("telemetry: OTLP export disabled: {err}");
                }
            }
        }
    }

    tracing_subscriber::fmt().with_env_filter(filter).init();
    Guard {
        #[cfg(feature = "otlp")]
        provider: None,
    }
}

#[cfg(feature = "otlp")]
fn otlp_layer(
    endpoint: &str,
) -> Result<opentelemetry_sdk::trace::SdkTracerProvider, Box<dyn std::error::Error>> {
    use opentelemetry_otlp::WithExportConfig as _;

    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_endpoint(endpoint)
        .build()?;
    Ok(opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(
            opentelemetry_sdk::Resource::builder()
                .with_service_name("lulan-api")
                .build(),
        )
        .build())
}

/// Flushes buffered spans on shutdown. Without this a crash-free exit
/// still loses whatever was in the batch queue — which is precisely the
/// tail you were tracing to find.
pub struct Guard {
    #[cfg(feature = "otlp")]
    provider: Option<opentelemetry_sdk::trace::SdkTracerProvider>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        #[cfg(feature = "otlp")]
        if let Some(provider) = self.provider.take()
            && let Err(err) = provider.shutdown()
        {
            eprintln!("telemetry: flushing spans on shutdown failed: {err}");
        }
    }
}
