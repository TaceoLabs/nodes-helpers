//! A pool from `pg_pool_with_schema` must not make `tracing` lose events.
//!
//! This file holds a single test because it installs the global `log` logger.
#![allow(clippy::unwrap_used, reason = "test code")]

use std::{
    io,
    sync::{Arc, Mutex},
};

use secrecy::SecretString;
use taceo_nodes_common::postgres::{CreateSchema, PostgresConfig, pg_pool_with_schema};
use tracing_subscriber::{
    EnvFilter, Layer as _, fmt::MakeWriter, layer::SubscriberExt as _, util::SubscriberInitExt as _,
};

/// Collects everything the `fmt` layer writes.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Captured {
    fn contents(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

impl io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Captured {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// With a per-layer filter that enables `DEBUG` for some target but not for `sqlx`, the first
/// event after a statement was dropped while `sqlx` logged statements
/// (<https://github.com/tokio-rs/tracing/issues/2519>).
///
/// Runs on the current-thread runtime, so that the statements and the events share a thread.
#[tokio::test]
async fn events_after_statements_are_not_lost() -> eyre::Result<()> {
    let captured = Captured::default();
    let filter = EnvFilter::new("postgres_statement_logging=trace,warn");
    // `init` also forwards `log` records to `tracing`, like the services do.
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(captured.clone())
                .with_filter(filter),
        )
        .init();

    let pg = taceo_nodes_common::test_utils::shared_postgres_testcontainer().await?;
    let config = PostgresConfig::with_default_values(
        SecretString::from(pg.connection_string.clone()),
        "statement_logging".parse()?,
    );
    let pool = pg_pool_with_schema(&config, CreateSchema::Yes).await?;

    for i in 0..3 {
        sqlx::query("SELECT 1").execute(&pool).await?;
        tracing::info!("after statement {i}");
    }

    let logged = captured.contents();
    for i in 0..3 {
        assert!(
            logged.contains(&format!("after statement {i}")),
            "the event after statement {i} is missing in:\n{logged}"
        );
    }
    Ok(())
}
