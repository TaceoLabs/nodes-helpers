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

/// Updatable cached nonce manager
///
/// Like `CachedNonceManager`, but `update` lets a caller update the cached nonce for an address
/// by fetching it from chain instead of incrementing the cached value. Used to recover from "nonce too low" errors,
/// which otherwise leave the cache out of sync forever (see `CachedNonceManager`'s doc comment).
#[derive(Clone, Debug, Default)]
pub struct UpdatableCachedNonceManager {
    /// Per-address cache of the *next* nonce to serve (not the last one served).
    nonces: Arc<DashMap<Address, Arc<Mutex<u64>>>>,
}

impl UpdatableCachedNonceManager {
    /// Updates the cached nonce for the given address by refetching it from chain and merging
    /// it into the cache via `max`, so the cache only ever moves forward.
    ///
    /// # Errors
    ///
    /// Returns an error if the provider fails to fetch the nonce from chain.
    pub async fn update<P, N>(&self, provider: &P, address: Address) -> TransportResult<()>
    where
        P: Provider<N>,
        N: Network,
    {
        if let Some(entry) = self.nonces.get(&address) {
            let nonce = Arc::clone(entry.value());
            drop(entry);
            let mut nonce = nonce.lock().await;
            let fetched = provider.get_transaction_count(address).pending().await?;
            *nonce = if *nonce == NONE {
                fetched
            } else {
                fetched.max(*nonce)
            };
        }
        Ok(())
    }
}

#[cfg_attr(target_family = "wasm", async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait)]
impl NonceManager for UpdatableCachedNonceManager {
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
        if *nonce == NONE {
            // Initialize the nonce if we haven't seen this account before.
            tracing::trace!(%address, "fetching nonce");
            *nonce = provider.get_transaction_count(address).pending().await?;
        } else {
            tracing::trace!(%address, next_nonce = *nonce, "using cached nonce");
        }
        let to_serve = *nonce;
        *nonce += 1;
        Ok(to_serve)
    }
}
