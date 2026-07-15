//! Transaction signer configurations for alloy.
//!
//! Each submodule provides a `*SignerConfig` type that can be deserialized
//! from configuration and turned into an alloy [`TxSigner`](alloy::network::TxSigner)
//! via an `into_signer` method, backed by a different key-management backend:
//!
//! - [`aws`]: signs using an AWS KMS key.
//! - [`gcp`]: signs using a GCP Cloud KMS key.
//! - [`local`]: signs using a private key held in memory.

/// Signer backed by an AWS KMS key.
#[cfg(feature = "signer-aws")]
pub mod aws;
/// Signer backed by a GCP Cloud KMS key.
#[cfg(feature = "signer-gcp")]
pub mod gcp;
/// Signer backed by a local, in-memory private key.
#[cfg(feature = "signer-local")]
pub mod local;
