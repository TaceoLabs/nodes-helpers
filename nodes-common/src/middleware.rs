//! Axum middleware layers.
//!
//! # Modules
//!
//! * [`unkey`] (requires the `unkey` feature) – Tower middleware that authenticates
//!   requests by verifying the `Authorization: Bearer` token against the
//!   [Unkey](https://unkey.com) API.

#[cfg(feature = "unkey")]
pub mod unkey;
