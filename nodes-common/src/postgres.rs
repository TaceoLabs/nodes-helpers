//! Configuration for a Postgres database.
//!
//! This module provides [`PostgresConfig`], which contains the arguments
//! required to connect to a `PostgreSQL` database using `sqlx`, and
//! [`pg_pool_with_schema`] to create a connection pool with an optional
//! schema auto-creation step.
//!
//! The struct supports:
//! - Required fields: `connection_string` and `schema`.
//! - Optional fields with sensible defaults (see below).
//! - Serde deserialization (with [`humantime_serde`] for durations).
//!
//! # Defaults
//!
//! | Field                    | Default  |
//! |--------------------------|----------|
//! | `max_connections`        | 4        |
//! | `acquire_timeout`        | 120 s    |
//! | `slow_acquire_threshold` | 90 s     |
//! | `max_retries`            | 20       |
//! | `retry_delay`            | 5 s      |
//!
//! # Example
//!
//! ```rust,no_run
//! use secrecy::SecretString;
//! use taceo_nodes_common::postgres::PostgresConfig;
//!
//! let config = PostgresConfig::with_default_values(
//!     SecretString::from("postgres://user:pass@localhost/db"),
//!     "my_schema".parse().unwrap(),
//! );
//! println!("{:?}", config);
//! ```
//!
//! Optional fields like `max_connections` or `retry_delay` are automatically
//! filled with defaults if not provided.
//!
//! # Retry behaviour
//!
//! [`pg_pool_with_schema`] retries pool creation using a **constant backoff**
//! strategy (powered by [`backon`]).  The interval between attempts is
//! [`PostgresConfig::retry_delay`] and at most [`PostgresConfig::max_retries`]
//! attempts are made before the error is propagated to the caller.
//!
//! Only *transient* errors are retried:
//!
//! | `sqlx::Error` variant | Reason |
//! |-----------------------|--------|
//! | `PoolTimedOut`        | All connections busy / pool exhausted |
//! | `Io`                  | Network-level failure |
//! | `Tls`                 | TLS handshake failure |
//! | `Protocol`            | Unexpected wire-protocol response |
//! | `AnyDriverError`      | Driver-specific transient error |
//! | `WorkerCrashed`       | Internal pool worker crashed |
//!
//! All other variants (e.g. `Configuration`, `ColumnDecode`, `Database`)
//! are considered **permanent** and cause an immediate failure.

use core::fmt;
use std::{
    num::{NonZeroU32, NonZeroUsize},
    str::FromStr,
    time::Duration,
};

use backon::{BackoffBuilder as _, ConstantBuilder, Retryable as _};
use secrecy::{ExposeSecret as _, SecretString};
use serde::{Deserialize, Deserializer, de};
use sqlx::{Executor as _, PgPool, postgres::PgPoolOptions};

fn deserialize_schema(s: &str) -> Result<SanitizedSchema, SanitizedSchemaParserError> {
    if s.is_empty() {
        return Err(SanitizedSchemaParserError);
    }
    if s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        Ok(SanitizedSchema(s.to_owned()))
    } else {
        Err(SanitizedSchemaParserError)
    }
}

/// A validated `PostgreSQL` schema name.
///
/// Only ASCII alphanumeric characters and underscores (`_`) are
/// allowed.  Use [`FromStr`], [`TryFrom<String>`], or serde deserialization
/// to construct an instance — all paths go through the same validation.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SanitizedSchema(String);

/// Error returned when a schema name fails validation.
///
/// See [`SanitizedSchema`] for the allowed character set.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct SanitizedSchemaParserError;

impl core::error::Error for SanitizedSchemaParserError {}

impl TryFrom<String> for SanitizedSchema {
    type Error = SanitizedSchemaParserError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl FromStr for SanitizedSchema {
    type Err = SanitizedSchemaParserError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        deserialize_schema(s)
    }
}

impl fmt::Display for SanitizedSchemaParserError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("schema must contain only ASCII alphanumeric and '_' and must not be empty")
    }
}

impl fmt::Display for SanitizedSchema {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for SanitizedSchema {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserialize_schema(&String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

/// Configuration for a `PostgreSQL` connection pool backed by `sqlx`.
///
/// See the [module-level documentation](self) for defaults and usage.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct PostgresConfig {
    /// Connection string for the database. Treat this as a secret.
    pub connection_string: SecretString,
    /// Database schema to use.  When [`CreateSchema::Yes`] is passed to
    /// [`pg_pool_with_schema`] and the schema does not exist yet, it is
    /// created automatically.
    pub schema: SanitizedSchema,
    /// Maximum number of connections in the connection pool.
    #[serde(default = "PostgresConfig::default_max_connections")]
    pub max_connections: NonZeroU32,
    /// Timeout for acquiring a connection from the pool.  If no connection
    /// becomes available within this duration, the acquire operation fails
    /// with [`sqlx::Error::PoolTimedOut`].
    #[serde(default = "PostgresConfig::default_acquire_timeout")]
    #[serde(with = "humantime_serde")]
    pub acquire_timeout: Duration,
    /// Threshold for `sqlx` warning logs.
    ///
    /// If acquiring a new connection from pool exceeds this threshold, `sqlx` will log a warning.
    #[serde(default = "PostgresConfig::default_slow_acquire_threshold")]
    #[serde(with = "humantime_serde")]
    pub slow_acquire_threshold: Duration,
    /// Maximum number of retry attempts when pool creation fails with a
    /// transient error (see [module-level docs](self#retry-behaviour) for
    /// the full list).  The database is considered unreachable once all
    /// retries are exhausted.
    #[serde(default = "PostgresConfig::default_max_retries")]
    pub max_retries: NonZeroUsize,
    /// Constant delay between retry attempts during pool creation (see
    /// [retry behaviour](self#retry-behaviour)).
    #[serde(default = "PostgresConfig::default_retry_delay")]
    #[serde(with = "humantime_serde")]
    pub retry_delay: Duration,
}

impl PostgresConfig {
    /// Default max connections
    fn default_max_connections() -> NonZeroU32 {
        NonZeroU32::try_from(4).expect("Is non-zero")
    }

    /// Default acquire timeout
    fn default_acquire_timeout() -> Duration {
        Duration::from_secs(120)
    }

    /// Default slow acquire threshold
    fn default_slow_acquire_threshold() -> Duration {
        Duration::from_secs(90)
    }

    /// Default max retries
    fn default_max_retries() -> NonZeroUsize {
        NonZeroUsize::try_from(20).expect("Is non-zero")
    }

    /// Default retry delay
    fn default_retry_delay() -> Duration {
        Duration::from_secs(5)
    }

    /// Construct with all default values except the required ones.
    ///
    /// Uses the following defaults:
    /// - `max_connections`: 4
    /// - `acquire_timeout`: 120 seconds
    /// - `slow_acquire_threshold`: 90 seconds
    /// - `max_retries`: 20
    /// - `retry_delay`: 5 seconds
    #[must_use]
    pub fn with_default_values(connection_string: SecretString, schema: SanitizedSchema) -> Self {
        Self {
            connection_string,
            schema,
            max_connections: Self::default_max_connections(),
            acquire_timeout: Self::default_acquire_timeout(),
            slow_acquire_threshold: Self::default_slow_acquire_threshold(),
            max_retries: Self::default_max_retries(),
            retry_delay: Self::default_retry_delay(),
        }
    }
}
#[must_use]
#[inline]
fn schema_connect_with_create(schema: &SanitizedSchema) -> String {
    format!(
        r#"
            CREATE SCHEMA IF NOT EXISTS "{schema}";
            SET search_path TO "{schema}";
        "#
    )
}
fn schema_connect(schema: &SanitizedSchema) -> String {
    format!(
        r#"
            SET search_path TO "{schema}";
        "#
    )
}

/// Whether to auto-create the schema if it does not exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[allow(clippy::exhaustive_enums, reason = "Is a boolean switch")]
pub enum CreateSchema {
    /// Run `CREATE SCHEMA IF NOT EXISTS` before setting the search path.
    Yes,
    /// Only set the search path; the schema must already exist.
    No,
}

/// Create a [`PgPool`] that pins every connection to `config.schema`.
///
/// When `create_schema` is [`CreateSchema::Yes`] the schema is created
/// (idempotently) on every new connection.
///
/// Pool creation is retried with a constant backoff (`config.retry_delay`
/// interval, up to `config.max_retries` attempts).  Only transient errors
/// are retried:
///
/// - [`sqlx::Error::PoolTimedOut`]
/// - [`sqlx::Error::Io`]
/// - [`sqlx::Error::Tls`]
/// - [`sqlx::Error::Protocol`]
/// - [`sqlx::Error::AnyDriverError`]
/// - [`sqlx::Error::WorkerCrashed`]
///
/// All other errors (e.g. `Configuration`, `ColumnDecode`) fail immediately.
///
/// # Errors
///
/// Returns [`sqlx::Error`] if pool creation fails after all retry attempts
/// are exhausted, or immediately for non-retryable errors.
pub async fn pg_pool_with_schema(
    config: &PostgresConfig,
    create_schema: CreateSchema,
) -> Result<PgPool, sqlx::Error> {
    let schema_connect = match create_schema {
        CreateSchema::Yes => schema_connect_with_create(&config.schema),
        CreateSchema::No => schema_connect(&config.schema),
    };

    let backoff_strategy = ConstantBuilder::new()
        .with_delay(config.retry_delay)
        .with_max_times(config.max_retries.get())
        .build();

    let pg_pool_options = PgPoolOptions::new()
        .max_connections(config.max_connections.get())
        .acquire_timeout(config.acquire_timeout)
        .acquire_slow_threshold(config.slow_acquire_threshold)
        .after_connect(move |conn, _| {
            let schema_connect = schema_connect.clone();
            Box::pin(async move {
                if let Err(e) = conn.execute(schema_connect.as_ref()).await {
                    tracing::error!("error in after_connect: {:?}", e);
                    return Err(e);
                }
                Ok(())
            })
        });
    (|| {
        pg_pool_options
            .clone()
            .connect(config.connection_string.expose_secret())
    })
    .retry(backoff_strategy)
    .sleep(tokio::time::sleep)
    .when(is_retryable_error)
    .notify(|e, duration| {
        tracing::warn!("Failed to create pool: {e:?}. Retry after {duration:?}");
    })
    .await
}

/// Returns `true` for transient `sqlx` errors that warrant a retry.
///
/// See the [module-level retry docs](self#retry-behaviour) for the full
/// rationale behind each variant.
#[inline]
fn is_retryable_error(e: &sqlx::Error) -> bool {
    matches!(
        e,
        sqlx::Error::PoolTimedOut
            | sqlx::Error::Io(_)
            | sqlx::Error::Tls(_)
            | sqlx::Error::Protocol(_)
            | sqlx::Error::AnyDriverError(_)
            | sqlx::Error::WorkerCrashed
    )
}
