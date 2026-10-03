use std::sync::atomic::{AtomicU64, Ordering};

use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) fn next_request_id() -> String {
    let sequence = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
    format!("{}-{sequence}", std::process::id())
}

pub(crate) struct Guard {
    #[cfg(feature = "otel")]
    provider: Option<opentelemetry_sdk::trace::SdkTracerProvider>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        #[cfg(feature = "otel")]
        if let Some(provider) = self.provider.take() {
            let _ = provider.shutdown();
        }
    }
}

pub(crate) fn init() -> Result<Guard, Box<dyn std::error::Error>> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let format = tracing_subscriber::fmt::layer()
        .json()
        .with_ansi(false)
        .with_writer(std::io::stderr);

    #[cfg(feature = "otel")]
    if otel_enabled() {
        use opentelemetry::trace::TracerProvider as _;

        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .build()?;
        let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
            .with_batch_exporter(exporter)
            .build();
        let tracer = provider.tracer(env!("CARGO_PKG_NAME"));
        tracing_subscriber::registry()
            .with(filter)
            .with(format)
            .with(tracing_opentelemetry::layer().with_tracer(tracer))
            .try_init()?;
        return Ok(Guard {
            provider: Some(provider),
        });
    }

    tracing_subscriber::registry()
        .with(filter)
        .with(format)
        .try_init()?;
    Ok(Guard {
        #[cfg(feature = "otel")]
        provider: None,
    })
}

#[cfg(feature = "otel")]
fn otel_enabled() -> bool {
    std::env::var("HACKMD_MCP_OTEL")
        .is_ok_and(|value| matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
}

#[cfg(test)]
mod tests {
    use super::next_request_id;

    #[test]
    fn request_ids_are_nonempty_and_unique() {
        let first = next_request_id();
        let second = next_request_id();
        assert!(!first.is_empty());
        assert_ne!(first, second);
    }
}
