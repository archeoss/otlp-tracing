use std::path::PathBuf;

use crate::cli::LoggerConfig;

use error_stack::{Context, Report, Result, ResultExt};
use file_rotate::{suffix::AppendTimestamp, ContentLimit, FileRotate};
use thiserror::Error;
use tower_http::cors::CorsLayer;
use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};
use tracing_subscriber::{filter::LevelFilter, prelude::*, util::SubscriberInitExt};

use autometrics::objectives::{Objective, ObjectiveLatency, ObjectivePercentile};
use opentelemetry::KeyValue;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::{runtime, trace as sdktrace, Resource};
use tracing_opentelemetry::OpenTelemetryLayer;

pub const API_SLO: Objective = Objective::new("api")
    // We expect 99.9% of all requests to succeed.
    .success_rate(ObjectivePercentile::P99_9)
    // We expect 99% of all latencies to be below 250ms.
    .latency(ObjectiveLatency::Ms250, ObjectivePercentile::P99);

pub trait LoggerExt {
    /// Initialize logger.
    ///
    /// Returns [`WorkerGuard`]s for off-thread writers.
    /// Should not be dropped.
    ///
    /// # Errors
    ///
    /// Function returns error if `init_file_rotate` fails
    fn init_logger(&self) -> Result<Vec<WorkerGuard>, LoggerError>;

    /// Returns [`std:io::Write`] object that rotates files on write
    ///
    /// # Errors
    ///
    /// Function returns error if `log_file` is not specified
    fn init_file_rotate(&self) -> Result<FileRotate<AppendTimestamp>, LoggerError>;

    /// Returns non-blocking file writer
    ///
    /// Also returns [`WorkerGuard`] for off-thread writing.
    /// Should not be dropped.
    ///
    /// # Errors
    ///
    /// This function will return an error if the file logger configuration is empty, file logging
    /// is disabled or logs filename is not specified
    fn non_blocking_file_writer(&self) -> Result<(NonBlocking, WorkerGuard), LoggerError>;

    /// Returns non-blocking stdout writer
    ///
    /// Also returns [`WorkerGuard`] for off-thread writing.
    /// Should not be dropped.
    ///
    /// # Errors
    ///
    /// This function will return an error if the stdout logger configuration is empty or stdout logging
    /// is disabled
    fn non_blocking_stdout_writer(&self) -> Result<(NonBlocking, WorkerGuard), LoggerError>;

    /// Init OTLP exporter
    fn init_metrics<
        S: tracing::Subscriber + for<'span> tracing_subscriber::registry::LookupSpan<'span>,
    >(
        &self,
    ) -> Result<OpenTelemetryLayer<S, opentelemetry_sdk::trace::Tracer>, LoggerError>;
}

impl LoggerExt for LoggerConfig {
    fn init_logger(&self) -> Result<Vec<WorkerGuard>, LoggerError> {
        let mut guards = Vec::with_capacity(2);

        let file_writer = disable_on_error(self.non_blocking_file_writer())?;
        let stdout_writer = disable_on_error(self.non_blocking_stdout_writer())?;

        let mut layers_iter =
            [file_writer, stdout_writer]
                .into_iter()
                .flatten()
                .map(|(writer, guard)| {
                    guards.push(guard);
                    tracing_subscriber::fmt::layer()
                        .with_writer(writer)
                        .with_filter(LevelFilter::from_level(self.trace_level))
                });

        if let Some(first_layer) = layers_iter.next() {
            let layers = layers_iter.fold(first_layer.boxed(), |layer, next_layer| {
                layer.and_then(next_layer).boxed()
            });
            let layers = layers.and_then(
                self.init_metrics()
                    .change_context(LoggerError::OLTPInitFailed)?,
            );
            tracing_subscriber::registry().with(layers).init();
        };

        Ok(guards)
    }

    fn init_file_rotate(&self) -> Result<FileRotate<AppendTimestamp>, LoggerError> {
        let config = self.file.as_ref().ok_or(LoggerError::EmptyConfig)?;
        let log_file = config.log_file.as_ref().ok_or(LoggerError::NoFileName)?;
        if log_file.as_os_str().is_empty() {
            return Err(LoggerError::NoFileName.into());
        }

        Ok(FileRotate::new(
            log_file,
            AppendTimestamp::default(file_rotate::suffix::FileLimit::MaxFiles(config.log_amount)),
            ContentLimit::BytesSurpassed(config.log_size),
            file_rotate::compression::Compression::OnRotate(1),
            None,
        ))
    }

    fn non_blocking_file_writer(&self) -> Result<(NonBlocking, WorkerGuard), LoggerError> {
        self.file.as_ref().map_or_else(
            || Err(LoggerError::EmptyConfig.into()),
            |config| {
                if config.enabled {
                    Ok(tracing_appender::non_blocking(self.init_file_rotate()?))
                } else {
                    Err(LoggerError::NotEnabled.into())
                }
            },
        )
    }

    fn non_blocking_stdout_writer(&self) -> Result<(NonBlocking, WorkerGuard), LoggerError> {
        self.stdout.as_ref().map_or_else(
            || Err(LoggerError::EmptyConfig.into()),
            |config| {
                if config.enabled {
                    Ok(tracing_appender::non_blocking(std::io::stdout()))
                } else {
                    Err(LoggerError::NotEnabled.into())
                }
            },
        )
    }

    fn init_metrics<
        S: tracing::Subscriber + for<'span> tracing_subscriber::registry::LookupSpan<'span>,
    >(
        &self,
    ) -> Result<OpenTelemetryLayer<S, opentelemetry_sdk::trace::Tracer>, LoggerError> {
        autometrics::prometheus_exporter::init();
        let tracer = opentelemetry_otlp::new_pipeline()
            .tracing()
            .with_exporter(
                opentelemetry_otlp::new_exporter()
                    .tonic()
                    .with_endpoint("http://localhost:4317"),
            )
            .with_trace_config(
                sdktrace::config()
                    .with_resource(Resource::new(vec![KeyValue::new("service.name", "bob")])),
            )
            .install_batch(runtime::Tokio)
            .change_context(LoggerError::OLTPInitFailed)?;

        let opentelemetry = tracing_opentelemetry::layer().with_tracer(tracer);

        Ok(opentelemetry)
    }
}

#[derive(Debug, Error)]
pub enum LoggerError {
    #[error("Empty logger configuration")]
    EmptyConfig,
    #[error("No filename specified")]
    NoFileName,
    #[error("This logger is not enabled")]
    NotEnabled,
    #[error("OLTP init failed")]
    OLTPInitFailed,
}

/// Consume some errors to produce empty logger
fn disable_on_error(
    logger: Result<(NonBlocking, WorkerGuard), LoggerError>,
) -> Result<Option<(NonBlocking, WorkerGuard)>, LoggerError> {
    Ok(match logger {
        Ok(writer) => Some(writer),
        Err(e) => match e.current_context() {
            LoggerError::NotEnabled | LoggerError::EmptyConfig => None,
            _ => return Err(e),
        },
    })
}
