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
    /// The native span processor could not be initialized.
    #[error("failed to initialize native OpenTelemetry tracing: {0}")]
    NativeProcessor(String),

    /// Native OpenTelemetry tracing is not supported on this platform.
    #[error("native OpenTelemetry tracing is not supported on this platform")]
    UnsupportedPlatform,
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

/// Creates a native OpenTelemetry layer and its provider guard.
///
/// Completed spans are written to ETW on Windows and `user_events` on Linux.
/// The returned guard must live as long as the subscriber and be dropped before
/// terminating the process.
pub fn init_native_layer<S>(
    service_name: &'static str,
    service_version: &'static str,
) -> Result<(OpenTelemetryLayer<S>, TracerProviderGuard), InitError>
where
    S: Subscriber + for<'span> LookupSpan<'span>,
{
    let resource = Resource::builder()
        .with_service_name(service_name)
        .with_attributes([KeyValue::new("service.version", service_version)])
        .build();
    let provider = SdkTracerProvider::builder()
        .with_span_processor(native_processor(service_name, &resource)?)
        .with_resource(resource)
        .build();

    let tracer = provider.tracer(service_name);
    let layer = tracing_opentelemetry::layer().with_tracer(tracer);

    Ok((layer, TracerProviderGuard { provider }))
}

#[cfg(windows)]
fn native_processor(
    service_name: &'static str,
    resource: &Resource,
) -> Result<opentelemetry_etw_traces::Processor, InitError> {
    opentelemetry_etw_traces::Processor::builder(service_name)
        .with_resource_attributes(part_c_resource_keys(resource))
        .build()
        .map_err(|error| InitError::NativeProcessor(error.to_string()))
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn native_processor(
    service_name: &'static str,
    _resource: &Resource,
) -> Result<opentelemetry_user_events_trace::Processor, InitError> {
    opentelemetry_user_events_trace::Processor::builder(service_name)
        .build()
        .map_err(|error| InitError::NativeProcessor(error.to_string()))
}

#[cfg(not(any(windows, all(target_os = "linux", target_env = "gnu"))))]
fn native_processor(
    _service_name: &'static str,
    _resource: &Resource,
) -> Result<UnsupportedProcessor, InitError> {
    Err(InitError::UnsupportedPlatform)
}

#[cfg(any(windows, test))]
fn part_c_resource_keys(resource: &Resource) -> impl Iterator<Item = String> + '_ {
    resource.iter().filter_map(|(key, _)| match key.as_str() {
        "service.name" | "service.instance.id" => None,
        key => Some(key.to_string()),
    })
}

#[cfg(not(any(windows, all(target_os = "linux", target_env = "gnu"))))]
#[derive(Debug)]
struct UnsupportedProcessor;

#[cfg(not(any(windows, all(target_os = "linux", target_env = "gnu"))))]
impl opentelemetry_sdk::trace::SpanProcessor for UnsupportedProcessor {
    fn on_start(&self, _span: &mut opentelemetry_sdk::trace::Span, _cx: &opentelemetry::Context) {}

    fn on_end(&self, _span: opentelemetry_sdk::trace::SpanData) {}

    fn force_flush(&self) -> opentelemetry_sdk::error::OTelSdkResult {
        Ok(())
    }

    fn shutdown_with_timeout(
        &self,
        _timeout: std::time::Duration,
    ) -> opentelemetry_sdk::error::OTelSdkResult {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::part_c_resource_keys;
    use opentelemetry::KeyValue;
    use opentelemetry_sdk::Resource;

    #[test]
    fn part_c_keys_include_non_part_a_resource_attributes() {
        let resource = Resource::builder_empty()
            .with_attributes([
                KeyValue::new("service.name", "openvmm"),
                KeyValue::new("service.instance.id", "run-42"),
                KeyValue::new("service.version", "1.0"),
                KeyValue::new("run.id", "boot-42"),
            ])
            .build();

        let mut keys = part_c_resource_keys(&resource).collect::<Vec<_>>();
        keys.sort();

        assert_eq!(keys, ["run.id", "service.version"]);
    }
}
