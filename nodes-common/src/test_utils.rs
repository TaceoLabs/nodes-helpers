//! Utilities for testing nodes.

use std::sync::atomic::{AtomicU64, Ordering};

use axum_test::{TestServer, transport_layer::IntoTransportLayer};
use eyre::Context;
use sqlx::{Connection as _, Executor as _, PgConnection};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, runners::AsyncRunner as _},
};
use tokio::sync::OnceCell;

use crate::postgres::SanitizedSchema;

pub use axum_test;

/// Returns a unique schema name for one test (`test_0`, `test_1`, …).
#[allow(
    clippy::missing_panics_doc,
    reason = "synthesized schema is always valid"
)]
pub fn next_test_schema() -> SanitizedSchema {
    static SCHEMA_COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = SCHEMA_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("test_{n}")
        .parse()
        .expect("synthesized schema is always valid")
}

/// Returns a new Postgres testcontainer and its connection string.
///
/// # Errors
///
/// Returns an error if the Postgres container could not be started
/// or if the connection string could not be constructed.
pub async fn postgres_testcontainer() -> eyre::Result<(ContainerAsync<Postgres>, String)> {
    let postgres_container = Postgres::default().start().await?;
    let connection_string = format!(
        "postgres://postgres:postgres@{}:{}/postgres",
        postgres_container.get_host().await?,
        postgres_container.get_host_port_ipv4(5432).await?
    );
    Ok((postgres_container, connection_string))
}

/// Returns a connection string to a process-wide shared Postgres container.
/// Started lazily on the first call; the container lives until process exit.
///
/// # Errors
///
/// Returns an error if the Postgres container could not be started.
pub async fn shared_postgres_testcontainer() -> eyre::Result<&'static str> {
    static SHARED_PG: OnceCell<(ContainerAsync<Postgres>, String)> = OnceCell::const_new();
    let shared = SHARED_PG
        .get_or_try_init(|| async {
            let (container, connection_string) = postgres_testcontainer().await?;
            eyre::Ok((container, connection_string))
        })
        .await?;
    Ok(&shared.1)
}

/// Wrapper function to get a random free port for tests without having to add the `reserve_port` crate directly.
///
/// # Errors
///
/// Returns an error if a free port could not be found or reserved.
pub fn random_port() -> eyre::Result<u16> {
    reserve_port::ReservedPort::random_permanently_reserved().context("while reserving port")
}

/// Returns a test server and its base URL for the given app.
///
/// # Panics
///
/// Panics if the test server could not be started.
pub fn test_server<A: IntoTransportLayer>(app: A) -> (TestServer, String) {
    let server = TestServer::builder().http_transport().build(app);
    let url = server
        .server_address()
        .expect("test server with http transport has address")
        .to_string()
        .trim_end_matches('/')
        .to_string();
    (server, url)
}

/// Creates a connection to the provided database and creates the
/// provided schema if it does not exist yet.
///
/// Useful when creating test-fixtures.
///
/// # Errors
/// If the creation of the schema fails for any reason (e.g,. DB connection issues)
pub async fn open_pg_connection(
    connection_string: &str,
    schema: &SanitizedSchema,
) -> eyre::Result<PgConnection> {
    let mut conn = PgConnection::connect(connection_string)
        .await
        .context("while opening PgConnection")?;

    conn.execute(format!("CREATE SCHEMA IF NOT EXISTS \"{schema}\"").as_ref())
        .await
        .context("TestUtils: cannot create schema")?;

    conn.execute(format!("SET search_path TO \"{schema}\"").as_ref())
        .await
        .context("TestUtils: cannot set search path of connection")?;
    Ok(conn)
}
