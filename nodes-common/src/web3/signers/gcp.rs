use alloy::signers::gcp::{
    GcpKeyRingRef, GcpSigner, GcpSignerError, KeySpecifier,
    gcloud_sdk::{
        self, GoogleApi,
        google::cloud::kms::v1::key_management_service_client::KeyManagementServiceClient,
    },
};
use serde::Deserialize;
use thiserror::Error;

/// Errors that can occur while turning a [`GcpSignerConfig`] into a signer.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum GcpSignerConfigError {
    /// Failed to build the underlying GCP Cloud KMS client.
    #[error("failed to build GCP Cloud KMS client: {0}")]
    GCloudSdk(#[from] gcloud_sdk::error::Error),
    /// Failed to create the signer from the configured key.
    #[error("failed to create GCP signer: {0}")]
    Signer(#[from] GcpSignerError),
}

/// Configuration for a signer backed by a GCP Cloud KMS key.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct GcpSignerConfig {
    /// The GCP project ID that owns the key ring.
    pub project_id: String,
    /// The location (region) of the key ring, e.g. `"global"` or `"europe-west3"`.
    pub location: String,
    /// The name of the key ring within the project and location.
    pub keyring: String,
    /// The name of the key within the key ring.
    pub key_name: String,
    /// The version of the key to sign with.
    pub key_version: u64,
    /// Optional chain ID enforced on transactions before signing.
    ///
    /// If `Some`, transactions signed by this signer must match this chain ID.
    /// If `None`, the signer does not check or set the transaction's chain ID.
    pub chain_id: Option<u64>,
}

impl GcpSignerConfig {
    /// Builds a [`GcpSigner`] from this configuration.
    ///
    /// # Errors
    ///
    /// Returns [`GcpSignerConfigError::GCloudSdk`] if the Cloud KMS client fails
    /// to connect, or [`GcpSignerConfigError::Signer`] if the signer fails to
    /// fetch the public key for the configured key version.
    pub async fn into_signer(self) -> Result<GcpSigner, GcpSignerConfigError> {
        let keyring = GcpKeyRingRef::new(&self.project_id, &self.location, &self.keyring);
        let client = GoogleApi::from_function(
            KeyManagementServiceClient::new,
            "https://cloudkms.googleapis.com",
            None,
        )
        .await?;
        let specifier = KeySpecifier::new(keyring, &self.key_name, self.key_version);
        let signer = GcpSigner::new(client, specifier, self.chain_id).await?;
        Ok(signer)
    }
}
