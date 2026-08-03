//! Abstraction over how a transaction actually gets signed and broadcast.
//!
//! [`TransactionHandler`] lets callers submit a transaction and wait for its
//! confirmation without caring whether it's sent through a plain alloy
//! provider + wallet (see the [`HttpRpcProvider`] impl below) or through an
//! account-abstraction wallet API, such as Alchemy's `wallet_*` RPC methods,
//! which sign and submit calls through an entirely different prepare → sign →
//! send → poll flow.

use alloy::{
    primitives::{TxHash, TxKind, U256},
    providers::{PendingTransactionError, Provider as _},
    rpc::types::TransactionRequest,
    signers::Signer,
    transports::TransportError,
};

use crate::web3::{HttpRpcProvider, transaction_handler::types::WalletCall};

pub mod alchemy_wallet_api;
pub mod types;

/// Submits a transaction and waits for it to be confirmed.
#[async_trait::async_trait]
pub trait TransactionHandler {
    /// The error returned when sending or confirming the transaction fails.
    type Error;

    /// Submits `tx` and waits until it is confirmed, returning its hash.
    async fn send_transaction(&self, tx: TransactionRequest) -> Result<TxHash, Self::Error>;
}

/// Errors returned by [`HttpRpcProvider`]'s [`TransactionHandler`] implementation.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum HttpRpcTransactionError {
    /// The transaction could not be submitted.
    #[error(transparent)]
    Send(#[from] TransportError),
    /// The transaction was submitted but never confirmed.
    #[error(transparent)]
    Confirm(#[from] PendingTransactionError),
}

#[async_trait::async_trait]
impl TransactionHandler for HttpRpcProvider {
    type Error = HttpRpcTransactionError;

    async fn send_transaction(&self, tx: TransactionRequest) -> Result<TxHash, Self::Error> {
        let provider = self.inner();
        let hash = provider.send_transaction(tx).await?.watch().await?;
        Ok(hash)
    }
}

#[async_trait::async_trait]
impl<S: Signer + Send + Sync> TransactionHandler for alchemy_wallet_api::AlchemyWalletApi<S> {
    type Error = eyre::Report;

    async fn send_transaction(&self, tx: TransactionRequest) -> Result<TxHash, Self::Error> {
        let Some(TxKind::Call(to)) = tx.to else {
            eyre::bail!("call has no target address (deployments are not supported)")
        };
        let value = tx.value.unwrap_or(U256::ZERO);
        let data = tx.input.into_input().unwrap_or_default();
        let call = WalletCall { to, data, value };

        let receipt = self.send_calls(vec![call]).await?;
        Ok(receipt.transaction_hash.parse()?)
    }
}
