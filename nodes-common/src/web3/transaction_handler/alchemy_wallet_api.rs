use std::time::Duration;

use alloy::primitives::Address;
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use alloy::signers::Signer;
use eyre::{Context as _, ContextCompat as _};
use serde_json::json;

use super::types::{
    CallReceipt, CallsStatus, PreparedItem, SendPreparedCallsResponse, SignedItem, WalletCall,
    prepare_params, to_send_param,
};

/// Client for Alchemy's Wallet API: gas-sponsored calls from an
/// EIP-7702-delegated EOA, generic over any alloy [`Signer`].
///
/// [`send_calls`](Self::send_calls) runs the whole
/// prepare → sign → send → poll flow; the individual steps are public too
/// for callers that want to drive it themselves.
pub struct AlchemyWalletApi<S> {
    provider: DynProvider,
    signer: S,
    chain_id: String,
    policy_id: String,
    nonce_key: Option<u64>,
    poll_interval: Duration,
    timeout: Duration,
}

impl<S: Signer> AlchemyWalletApi<S> {
    #[must_use]
    pub fn builder() -> AlchemyWalletApiBuilder<S> {
        AlchemyWalletApiBuilder::default()
    }

    pub fn address(&self) -> Address {
        self.signer.address()
    }

    /// Full flow: prepare → sign → send → wait for confirmation.
    pub async fn send_calls(&self, calls: Vec<WalletCall>) -> eyre::Result<CallReceipt> {
        let items = self.prepare_calls(calls).await?;
        let signed = self.sign_prepared(items).await?;
        let call_id = self.send_prepared_calls(signed).await?;
        self.wait_for_confirmation(&call_id).await
    }

    /// `wallet_prepareCalls`: returns the item(s) to sign — an EIP-7702
    /// authorization plus a user operation while the EOA is not yet
    /// delegated, just a user operation afterwards.
    pub async fn prepare_calls(&self, calls: Vec<WalletCall>) -> eyre::Result<Vec<PreparedItem>> {
        let prepared: PreparedItem = self
            .provider
            .client()
            .request(
                "wallet_prepareCalls",
                prepare_params(
                    self.address(),
                    &self.chain_id,
                    calls,
                    &self.policy_id,
                    self.nonce_key,
                ),
            )
            .await
            .context("wallet_prepareCalls failed")?;
        prepared.into_items()
    }

    /// Sign every prepared item with this wallet's signer.
    pub async fn sign_prepared(&self, items: Vec<PreparedItem>) -> eyre::Result<Vec<SignedItem>> {
        let mut signed = Vec::with_capacity(items.len());
        for item in items {
            signed.push(SignedItem::from_prepared(item, &self.signer).await?);
        }
        Ok(signed)
    }

    /// `wallet_sendPreparedCalls`: submit and return the call ID to poll.
    pub async fn send_prepared_calls(&self, signed: Vec<SignedItem>) -> eyre::Result<String> {
        let sent: SendPreparedCallsResponse = self
            .provider
            .client()
            .request("wallet_sendPreparedCalls", json!([to_send_param(signed)?]))
            .await
            .context("wallet_sendPreparedCalls failed")?;
        sent.prepared_call_ids
            .into_iter()
            .next()
            .context("no preparedCallIds in response")
    }

    /// `wallet_getCallsStatus`: single status poll.
    pub async fn get_calls_status(&self, call_id: &str) -> eyre::Result<CallsStatus> {
        self.provider
            .client()
            .request("wallet_getCallsStatus", json!([call_id]))
            .await
            .context("wallet_getCallsStatus failed")
    }

    /// Poll every `poll_interval` until confirmed (status 200), a terminal
    /// status, or `timeout` elapses.
    pub async fn wait_for_confirmation(&self, call_id: &str) -> eyre::Result<CallReceipt> {
        tokio::time::timeout(self.timeout, async {
            loop {
                let status = self.get_calls_status(call_id).await?;
                match status.status {
                    100..=199 => tokio::time::sleep(self.poll_interval).await,
                    200 => {
                        return status
                            .receipts
                            .and_then(|r| r.into_iter().next())
                            .context("confirmed but no receipt in response");
                    }
                    code => eyre::bail!("call failed with wallet_getCallsStatus status {code}"),
                }
            }
        })
        .await
        .with_context(|| format!("no confirmation within {:?}", self.timeout))?
    }
}

pub struct AlchemyWalletApiBuilder<S> {
    api_key: Option<String>,
    chain_id: Option<String>,
    policy_id: Option<String>,
    signer: Option<S>,
    nonce_key: Option<u64>,
    poll_interval: Duration,
    timeout: Duration,
}

impl<S> Default for AlchemyWalletApiBuilder<S> {
    fn default() -> Self {
        Self {
            api_key: None,
            chain_id: None,
            policy_id: None,
            signer: None,
            nonce_key: None,
            poll_interval: Duration::from_secs(3),
            timeout: Duration::from_secs(180),
        }
    }
}

impl<S: Signer> AlchemyWalletApiBuilder<S> {
    /// Alchemy API key (bare key, not a URL).
    #[must_use]
    pub fn api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }

    /// Hex chain ID, e.g. `0xaa36a7` for Sepolia.
    #[must_use]
    pub fn chain_id(mut self, chain_id: impl Into<String>) -> Self {
        self.chain_id = Some(chain_id.into());
        self
    }

    /// Gas Manager sponsorship policy UUID.
    #[must_use]
    pub fn policy_id(mut self, policy_id: impl Into<String>) -> Self {
        self.policy_id = Some(policy_id.into());
        self
    }

    #[must_use]
    pub fn signer(mut self, signer: S) -> Self {
        self.signer = Some(signer);
        self
    }

    /// EntryPoint 2D-nonce key (Alchemy `nonceOverride` capability). Give
    /// each concurrent sender for the same EOA a distinct nonzero key so
    /// their operations don't collide; note Alchemy's bundler allows at most
    /// 4 parallel nonce keys in flight for unstaked senders.
    #[must_use]
    pub fn nonce_key(mut self, nonce_key: u64) -> Self {
        self.nonce_key = Some(nonce_key);
        self
    }

    /// Delay between status polls (default 3 s).
    #[must_use]
    pub fn poll_interval(mut self, poll_interval: Duration) -> Self {
        self.poll_interval = poll_interval;
        self
    }

    /// Overall confirmation deadline (default 180 s).
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn build(self) -> eyre::Result<AlchemyWalletApi<S>> {
        let api_key = self.api_key.context("api_key is required")?;
        // The wallet_* methods live on a chain-agnostic endpoint; the chain
        // is selected per request via chainId.
        let url = format!("https://api.g.alchemy.com/v2/{api_key}");
        let provider =
            ProviderBuilder::new().connect_http(url.parse().context("bad wallet API URL")?);
        Ok(AlchemyWalletApi {
            provider: provider.erased(),
            signer: self.signer.context("signer is required")?,
            chain_id: self.chain_id.context("chain_id is required")?,
            policy_id: self.policy_id.context("policy_id is required")?,
            nonce_key: self.nonce_key,
            poll_interval: self.poll_interval,
            timeout: self.timeout,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::signers::local::PrivateKeySigner;

    #[test]
    fn builder_requires_mandatory_fields() {
        let err = AlchemyWalletApi::<PrivateKeySigner>::builder()
            .build()
            .err()
            .unwrap();
        assert!(err.to_string().contains("api_key"));

        let err = AlchemyWalletApi::<PrivateKeySigner>::builder()
            .api_key("key")
            .chain_id("0xaa36a7")
            .policy_id("policy")
            .build()
            .err()
            .unwrap();
        assert!(err.to_string().contains("signer"));
    }

    #[test]
    fn builder_applies_defaults_and_overrides() {
        let signer = PrivateKeySigner::random();
        let address = signer.address();
        let wallet = AlchemyWalletApi::builder()
            .api_key("key")
            .chain_id("0xaa36a7")
            .policy_id("policy")
            .signer(signer)
            .build()
            .unwrap();
        assert_eq!(wallet.poll_interval, Duration::from_secs(3));
        assert_eq!(wallet.timeout, Duration::from_secs(180));
        assert_eq!(wallet.nonce_key, None);
        assert_eq!(wallet.address(), address);

        let wallet = AlchemyWalletApi::builder()
            .api_key("key")
            .chain_id("0xaa36a7")
            .policy_id("policy")
            .signer(PrivateKeySigner::random())
            .poll_interval(Duration::from_millis(500))
            .timeout(Duration::from_secs(30))
            .nonce_key(7)
            .build()
            .unwrap();
        assert_eq!(wallet.poll_interval, Duration::from_millis(500));
        assert_eq!(wallet.timeout, Duration::from_secs(30));
        assert_eq!(wallet.nonce_key, Some(7));
    }
}
