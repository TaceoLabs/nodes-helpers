use std::str::FromStr;

use alloy::signers::{
    Signer as _,
    local::{LocalSignerError, PrivateKeySigner},
};
use secrecy::{ExposeSecret as _, SecretString};
use serde::Deserialize;

/// Configuration for a signer backed by a raw, in-memory private key.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct PrivateKeySignerConfig {
    /// The hex-encoded private key, kept as a [`SecretString`] to avoid
    /// accidental exposure in logs or debug output.
    pub private_key: SecretString,
    /// Optional chain ID enforced on transactions before signing.
    ///
    /// If `Some`, transactions signed by this signer must match this chain ID.
    /// If `None`, the signer does not check or set the transaction's chain ID.
    pub chain_id: Option<u64>,
}

impl PrivateKeySignerConfig {
    /// Builds a [`PrivateKeySigner`] from the configured private key.
    ///
    /// # Errors
    ///
    /// Returns an error if `private_key` is not a valid hex-encoded private key.
    pub fn into_signer(self) -> Result<PrivateKeySigner, LocalSignerError> {
        PrivateKeySigner::from_str(self.private_key.expose_secret())
            .map(|signer| signer.with_chain_id(self.chain_id))
    }
}
