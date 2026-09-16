// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! OpenTelemetry tracing initialization.

#![forbid(unsafe_code)]

use opentelemetry::KeyValue;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::SdkTracer;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing::Subscriber;
use tracing_subscriber::registry::LookupSpan;

/// An OpenTelemetry layer backed by the SDK tracer.
pub type OpenTelemetryLayer<S> = tracing_opentelemetry::OpenTelemetryLayer<S, SdkTracer>;

/// An error returned while initializing OpenTelemetry tracing.
#[derive(Debug, thiserror::Error)]
pub enum InitError {
    /// The OTLP exporter could not be configured.
    #[error("failed to build the OTLP span exporter")]
    Exporter(#[source] opentelemetry_otlp::ExporterBuildError),
}

/// Keeps the OpenTelemetry provider alive and flushes it when dropped.
#[derive(Debug)]
pub struct TracerProviderGuard {
    provider: SdkTracerProvider,
}

impl Drop for TracerProviderGuard {
    fn drop(&mut self) {
        if let Err(error) = self.provider.shutdown() {
            tracing::error!(
                error = &error as &dyn std::error::Error,
                "failed to shut down the OpenTelemetry tracer provider"
            );
        }
    }
}

/// Creates an OTLP/HTTP OpenTelemetry layer and its provider guard.
///
/// The exporter uses the standard `OTEL_EXPORTER_OTLP_*` environment
/// variables. The returned guard must live as long as the subscriber and be
/// dropped before terminating the process.
pub fn init_otlp_layer<S>(
    service_name: &'static str,
    service_version: &'static str,
) -> Result<(OpenTelemetryLayer<S>, TracerProviderGuard), InitError>
where
    S: Subscriber + for<'span> LookupSpan<'span>,
{
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .build()
        .map_err(InitError::Exporter)?;

    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(
            Resource::builder()
                .with_service_name(service_name)
                .with_attributes([KeyValue::new("service.version", service_version)])
                .build(),
        )
        .build();

    let tracer = provider.tracer(service_name);
    let layer = tracing_opentelemetry::layer().with_tracer(tracer);

    Ok((layer, TracerProviderGuard { provider }))
}

#[cfg(test)]
mod tests {
    use super::init_otlp_layer;
    use tracing_subscriber::Registry;

    #[test]
    fn initializes_and_shuts_down() {
        let (layer, guard) = init_otlp_layer::<Registry>("otel-tracing-test", "0.0.0").unwrap();
        drop(layer);
        drop(guard);
    }
}
