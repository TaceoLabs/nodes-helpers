//! Custom nonce manager implementations.

use std::sync::Arc;

use alloy::{
    network::Network,
    primitives::Address,
    providers::{Provider, fillers::NonceManager},
    transports::TransportResult,
};
use async_trait::async_trait;
use dashmap::DashMap;
use tokio::sync::Mutex;

// Use `u64::MAX` as a sentinel value to indicate that the nonce has not been fetched yet.
const NONE: u64 = u64::MAX;

/// Invalidatable cached nonce manager
///
/// Like `CachedNonceManager`, but `invalidate` lets a caller invalidate the cached nonce for an address
/// and force the next call to `get_next_nonce` to fetch it from chain instead of incrementing the cached value.
/// Used to recover from "nonce too low" errors, which otherwise leave the cache out of sync forever (see `CachedNonceManager`'s doc comment).
#[derive(Clone, Debug, Default)]
pub struct InvalidatableCachedNonceManager {
    nonces: Arc<DashMap<Address, Arc<Mutex<u64>>>>,
}

impl InvalidatableCachedNonceManager {
    /// Invalidate the cached nonce for the given address, forcing the next call to `get_next_nonce`
    /// to fetch it from chain instead of incrementing the cached value.
    pub async fn invalidate(&self, address: Address) {
        if let Some(entry) = self.nonces.get(&address) {
            let nonce = Arc::clone(entry.value());
            drop(entry);
            *nonce.lock().await = NONE;
        }
    }
}

#[cfg_attr(target_family = "wasm", async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait)]
impl NonceManager for InvalidatableCachedNonceManager {
    async fn get_next_nonce<P, N>(&self, provider: &P, address: Address) -> TransportResult<u64>
    where
        P: Provider<N>,
        N: Network,
    {
        // Locks dashmap internally for a short duration to clone the `Arc`.
        // We also don't want to hold the dashmap lock through the await point below.
        let nonce = {
            let rm = self
                .nonces
                .entry(address)
                .or_insert_with(|| Arc::new(Mutex::new(NONE)));
            Arc::clone(rm.value())
        };

        let mut nonce = nonce.lock().await;
        let new_nonce = if *nonce == NONE {
            // Initialize the nonce if we haven't seen this account before.
            tracing::trace!(%address, "fetching nonce");
            provider.get_transaction_count(address).pending().await?
        } else {
            tracing::trace!(%address, current_nonce = *nonce, "incrementing nonce");
            *nonce + 1
        };
        *nonce = new_nonce;
        Ok(new_nonce)
    }
}
