use alloy::signers::aws::{AwsSigner, AwsSignerError, aws_config, aws_sdk_kms};
use serde::Deserialize;

/// Configuration for a signer backed by an AWS KMS key.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct AwsSignerConfig {
    /// The ID (or ARN/alias) of the AWS KMS key to sign with.
    pub key_id: String,
    /// Optional chain ID enforced on transactions before signing.
    ///
    /// If `Some`, transactions signed by this signer must match this chain ID.
    /// If `None`, the signer does not check or set the transaction's chain ID.
    pub chain_id: Option<u64>,
}

impl AwsSignerConfig {
    /// Builds a [`TxSigner`] backed by AWS KMS from this configuration.
    ///
    /// # Errors
    ///
    /// Returns an error if the AWS KMS client fails to fetch the public key
    /// or otherwise initialize the signer for the configured `key_id`.
    pub async fn into_signer(
        self,
        sdk_config: &aws_config::SdkConfig,
    ) -> Result<AwsSigner, AwsSignerError> {
        let client = aws_sdk_kms::Client::new(sdk_config);
        let signer = AwsSigner::new(client, self.key_id, self.chain_id).await?;
        Ok(signer)
    }
}
