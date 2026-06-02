//! On-chain event streaming with automatic historical backfill.
//!
//! This module provides a builder-based API for subscribing to smart-contract
//! events via WebSocket while optionally backfilling historical events over
//! HTTP. The backfill uses configurable block-range chunks so that large
//! histories do not produce oversized RPC requests.
//!
//! Use [`EventStreamBuilder`] to configure and create an event stream.
//! A [`ChainCursor`] tracks the last-processed position on chain so that
//! backfill resumes from the correct block and log index.
//!
//! # Stream flow
//!
//! When [`EventStreamBuilder::build`] is called the following steps are
//! executed in order:
//!
//! 1. **Subscribe to live logs** — calls `eth_subscribe("logs")` on the WS
//!    provider filtered by contract address and event topic, starting from
//!    `Latest`. Events are buffered in a broadcast channel of size
//!    `channel_size`.
//! 2. **Check for early return** — if the cursor is at genesis (block 0,
//!    index 0) or `skip_backfill` is `Yes`, the live WS stream is returned
//!    immediately with no backfill.
//! 3. **Determine the backfill cutoff** — subscribes to new block headers on
//!    the WS provider and waits for `confirmations_after_sync_block` headers.
//!    The **first** header's block number becomes the **backfill cutoff**.
//!    The additional headers give the HTTP provider time to reach the same
//!    block. This step is bounded by `new_head_timeout`; exceeding it yields
//!    [`EventStreamError::CannotFetchHead`].
//! 4. **Synchronize HTTP with WS** — polls `get_block_number` on the HTTP
//!    provider every `sync_poll_interval` until it reports a block number
//!    ≥ the cutoff. Bounded by `sync_timeout`; exceeding it yields
//!    [`EventStreamError::SynchronizingHttpWsTimeout`].
//! 5. **Backfill historical logs** — fetches logs over HTTP from
//!    `cursor.block` to the cutoff in chunks of `chunk_size` blocks via
//!    `eth_getLogs`. Logs whose `(block_number, log_index)` is at or before
//!    the cursor position are filtered out.
//! 6. **Chain backfill with live stream** — the backfill stream is chained
//!    with the WS stream. The WS stream drops any log whose block number
//!    ≤ the cutoff to avoid duplicates with the backfill.
//!
//! # Builder defaults
//!
//! | Parameter | Default | Effect |
//! |---|---|---|
//! | `channel_size` | 1024 | Broadcast channel capacity for WS events. Overflow yields [`EventStreamError::Lagging`]. |
//! | `chunk_size` | 1024 | Blocks per `eth_getLogs` request during backfill. Smaller = smaller RPC responses; larger = fewer round-trips. |
//! | `new_head_timeout` | 1 min | Max wait for `confirmations_after_sync_block` block headers from WS. |
//! | `sync_timeout` | 20 s | Max wait for HTTP provider to reach the cutoff block. |
//! | `sync_poll_interval` | 2 s | Interval between `get_block_number` polls during HTTP/WS sync. |
//! | `confirmations_after_sync_block` | 5 | Block headers to observe from WS before starting backfill. First header sets the cutoff; the rest are wait time. |
//! | `skip_backfill` | `No` | When `Yes`, skip backfill entirely and deliver only live events. |
//!
//! # Public types
//!
//! * [`EventStreamBuilder`] – configures and builds the event stream.
//! * [`ChainCursor`] – tracks the last-processed on-chain position.
//! * [`SkipBackfill`] – controls whether historical backfill is performed.
//! * [`EventStreamConfig`] – optional configuration values; can be
//!   deserialized from a config file and applied in bulk via
//!   [`EventStreamBuilder::with_config`].
//! * [`EventStreamError`] – errors produced while building or consuming the
//!   stream.

use std::ops::RangeInclusive;
use std::{fmt, num::NonZeroUsize, time::Duration};

use alloy::{
    eips::BlockNumberOrTag,
    primitives::{Address, FixedBytes},
    providers::{DynProvider, Provider},
    rpc::types::{Filter, FilterSet, Log, Topic},
};
use futures::{
    Stream,
    stream::{self, StreamExt, TryStreamExt},
};
use serde::Deserialize;
use tokio::sync::broadcast::error::RecvError;

use crate::web3;

/// Tracks the last-processed on-chain position.
///
/// A cursor is defined by a block number and the log index within that
/// block. The default cursor points to genesis (block 0, index 0), which
/// signals that no history needs to be replayed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ChainCursor {
    block: u64,
    index: u64,
}

impl fmt::Display for ChainCursor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_genesis() {
            f.write_str("Cursor(genesis)")
        } else {
            f.write_fmt(format_args!(
                "Cursor(block={}, idx={})",
                self.block, self.index
            ))
        }
    }
}

/// Errors that can occur while building or consuming an event stream.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum EventStreamError {
    /// The WebSocket subscription fell behind the broadcast channel capacity.
    #[error("eth_subscribe stream lagging behind - maybe backfill took too long")]
    Lagging,
    /// Subscribing to newHeads failed or ran into timeout.
    #[error("Cannot fetch newHead")]
    CannotFetchHead,
    /// Synchronizing between HTTP and WS ran into timeout
    #[error("Synchronizing between HTTP and WS ran into timeout")]
    SynchronizingHttpWsTimeout,
    /// A log received from the RPC was missing its block number.
    #[error("missing block number on log")]
    BlockNumberMissing,
    /// A log received from the RPC was missing its log index.
    #[error("missing index number on log")]
    IndexNumberMissing,
    /// An underlying RPC transport error.
    #[error(transparent)]
    TransportError(#[from] alloy::transports::TransportError),
}

impl ChainCursor {
    /// Creates a new cursor at the given block number and log index.
    #[must_use]
    pub fn new(block: u64, index: u64) -> Self {
        Self { block, index }
    }

    /// Returns the block number.
    #[inline]
    #[must_use]
    pub fn block(&self) -> u64 {
        self.block
    }

    /// Returns the log index within the block.
    #[inline]
    #[must_use]
    pub fn index(&self) -> u64 {
        self.index
    }

    /// Returns `true` if the cursor is at genesis (block 0, index 0).
    #[inline]
    #[must_use]
    pub fn is_genesis(&self) -> bool {
        self.block == 0 && self.index == 0
    }

    /// Returns `true` if `self` is strictly before `other`.
    #[inline]
    #[must_use]
    pub fn is_before(self, other: Self) -> bool {
        self < other
    }
}

/// Controls whether historical backfill is skipped when building an event
/// stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[allow(
    clippy::exhaustive_enums,
    reason = "Enum is either yes or no - not planned to extend this"
)]
#[serde(rename_all = "lowercase")]
pub enum SkipBackfill {
    /// Skip backfill; only receive live events.
    Yes,
    /// Perform backfill from the cursor position (default).
    #[default]
    No,
}

impl From<bool> for SkipBackfill {
    fn from(value: bool) -> Self {
        if value {
            SkipBackfill::Yes
        } else {
            SkipBackfill::No
        }
    }
}

/// Builder for creating an event stream that combines historical backfill
/// with live WebSocket events.
///
/// The resulting stream first replays historical logs fetched over HTTP
/// (from the [`ChainCursor`] position up to the current block), then
/// seamlessly continues with live events received via `eth_subscribe`.
/// Block ranges during backfill are split into configurable chunks to avoid
/// oversized RPC responses.
///
/// See the [module-level documentation](self) for the full flow description
/// and a table of all configurable defaults.
pub struct EventStreamBuilder<T> {
    chain_cursor: ChainCursor,
    contract_address: Address,
    http_provider: web3::HttpRpcProvider,
    ws_provider: DynProvider,
    topic: T,
    config: EventStreamConfig,
}

/// Configuration for [`EventStreamBuilder`].
///
/// Every field has a built-in default (see the [module-level defaults
/// table](self#builder-defaults)). The struct derives [`Deserialize`] so
/// it can be loaded from a config file and passed to
/// [`EventStreamBuilder::with_config`].
#[non_exhaustive]
#[derive(Debug, Clone, Deserialize)]
pub struct EventStreamConfig {
    /// See [`EventStreamBuilder::skip_backfill`].
    #[serde(default)]
    pub skip_backfill: SkipBackfill,
    /// See [`EventStreamBuilder::channel_size`].
    #[serde(default = "EventStreamConfig::default_channel_size")]
    pub channel_size: NonZeroUsize,
    /// See [`EventStreamBuilder::chunk_size`].
    #[serde(default = "EventStreamConfig::default_chunk_size")]
    pub chunk_size: NonZeroUsize,
    /// See [`EventStreamBuilder::new_head_timeout`].
    #[serde(default = "EventStreamConfig::default_new_head_timeout")]
    #[serde(with = "humantime_serde")]
    pub new_head_timeout: Duration,
    /// See [`EventStreamBuilder::sync_timeout`].
    #[serde(default = "EventStreamConfig::default_sync_timeout")]
    #[serde(with = "humantime_serde")]
    pub sync_timeout: Duration,
    /// See [`EventStreamBuilder::sync_poll_interval`].
    #[serde(default = "EventStreamConfig::default_sync_poll_interval")]
    #[serde(with = "humantime_serde")]
    pub sync_poll_interval: Duration,
    /// See [`EventStreamBuilder::confirmations_after_sync_block`].
    #[serde(default = "EventStreamConfig::default_confirmations_after_sync_block")]
    pub confirmations_after_sync_block: NonZeroUsize,
}

impl EventStreamConfig {
    /// Creates a new configuration using default settings.
    #[must_use]
    pub fn with_default_values() -> Self {
        Self::default()
    }

    fn default_channel_size() -> NonZeroUsize {
        NonZeroUsize::new(1024).expect("1024 is non-zero")
    }

    fn default_chunk_size() -> NonZeroUsize {
        NonZeroUsize::new(1024).expect("1024 is non-zero")
    }

    fn default_new_head_timeout() -> Duration {
        Duration::from_secs(60)
    }

    fn default_sync_timeout() -> Duration {
        Duration::from_secs(20)
    }

    fn default_sync_poll_interval() -> Duration {
        Duration::from_secs(2)
    }

    fn default_confirmations_after_sync_block() -> NonZeroUsize {
        NonZeroUsize::new(5).expect("5 is non-zero")
    }
}

impl Default for EventStreamConfig {
    fn default() -> Self {
        Self {
            skip_backfill: SkipBackfill::default(),
            channel_size: Self::default_channel_size(),
            chunk_size: Self::default_chunk_size(),
            new_head_timeout: Self::default_new_head_timeout(),
            sync_timeout: Self::default_sync_timeout(),
            sync_poll_interval: Self::default_sync_poll_interval(),
            confirmations_after_sync_block: Self::default_confirmations_after_sync_block(),
        }
    }
}

impl<T> EventStreamBuilder<T>
where
    T: Into<Topic>,
{
    /// Creates a new builder for the given contract and event topic.
    ///
    /// `topic` is the event signature hash (or a collection of hashes) used
    /// to filter logs on the RPC side.
    #[must_use]
    pub fn new(
        chain_cursor: ChainCursor,
        contract_address: Address,
        http_provider: web3::HttpRpcProvider,
        ws_provider: DynProvider,
        topic: T,
    ) -> Self {
        Self::with_config(
            chain_cursor,
            contract_address,
            http_provider,
            ws_provider,
            topic,
            EventStreamConfig::default(),
        )
    }

    /// Creates a new builder with the given configuration.
    #[must_use]
    pub fn with_config(
        chain_cursor: ChainCursor,
        contract_address: Address,
        http_provider: web3::HttpRpcProvider,
        ws_provider: DynProvider,
        topic: T,
        config: EventStreamConfig,
    ) -> Self {
        Self {
            chain_cursor,
            contract_address,
            http_provider,
            ws_provider,
            topic,
            config,
        }
    }

    /// Sets whether historical backfill should be skipped.
    #[must_use]
    pub fn skip_backfill(mut self, skip_backfill: SkipBackfill) -> Self {
        self.config.skip_backfill = skip_backfill;
        self
    }

    /// Sets the timeout for waiting on the next block header from the WS
    /// provider when determining the backfill cutoff.
    #[must_use]
    pub fn new_head_timeout(mut self, new_head_timeout: Duration) -> Self {
        self.config.new_head_timeout = new_head_timeout;
        self
    }

    /// Sets the timeout for waiting until WS and HTTP are synced.
    #[must_use]
    pub fn sync_timeout(mut self, sync_timeout: Duration) -> Self {
        self.config.sync_timeout = sync_timeout;
        self
    }

    /// Sets the poll interval which the HTTP provider fetches its current block number.
    ///
    /// This is used for the HTTP provider to poll the current block to synchronize with the WS provider cutoff block.
    #[must_use]
    pub fn sync_poll_interval(mut self, sync_poll_interval: Duration) -> Self {
        self.config.sync_poll_interval = sync_poll_interval;
        self
    }

    /// Sets the capacity of the WebSocket broadcast channel.
    ///
    /// If the consumer falls behind by more than this many events the stream
    /// yields [`EventStreamError::Lagging`].
    #[must_use]
    pub fn channel_size(mut self, channel_size: NonZeroUsize) -> Self {
        self.config.channel_size = channel_size;
        self
    }

    /// Sets the block-range chunk size used during backfill.
    #[must_use]
    pub fn chunk_size(mut self, chunk_size: NonZeroUsize) -> Self {
        self.config.chunk_size = chunk_size;
        self
    }

    /// Sets the number of new block headers to observe from the WS provider
    /// before starting the backfill. The first header's block number becomes
    /// the backfill cutoff; the remaining headers give the HTTP provider time
    /// to reach the same block.
    ///
    /// Higher values reduce the chance of a
    /// [`EventStreamError::SynchronizingHttpWsTimeout`] but delay the first
    /// log delivery.
    #[must_use]
    pub fn confirmations_after_sync_block(
        mut self,
        confirmations_after_sync_block: NonZeroUsize,
    ) -> Self {
        self.config.confirmations_after_sync_block = confirmations_after_sync_block;
        self
    }

    /// Builds the event stream.
    ///
    /// Subscribes to live events via WebSocket and, unless the cursor is at
    /// genesis or backfill is skipped, fetches historical logs over HTTP
    /// before chaining them with the live stream. See the
    /// [module-level documentation](self) for the full step-by-step flow.
    ///
    /// # Note
    /// This method subscribes to `newHeads` to get the current cutoff for reading
    /// events. Afterwards it unsubscribes the `newHeads` subscription again. Due to
    /// how alloy implements subscriptions, all `newHeads` subscriptions on the
    /// provided `ws_provider` will also be cancelled. If this is not acceptable build
    /// a new `Provider` for this call.
    ///
    /// # Errors
    ///
    /// - [`EventStreamError::TransportError`] — WS subscription or HTTP
    ///   log fetch failed.
    /// - [`EventStreamError::CannotFetchHead`] — timed out waiting for new
    ///   block headers from the WS provider.
    /// - [`EventStreamError::SynchronizingHttpWsTimeout`] — HTTP provider
    ///   did not reach the cutoff block in time.
    pub async fn build(
        self,
    ) -> Result<impl Stream<Item = Result<Log, EventStreamError>>, EventStreamError> {
        let Self {
            chain_cursor,
            contract_address,
            http_provider,
            ws_provider,
            topic,
            config,
        } = self;

        let EventStreamConfig {
            skip_backfill,
            channel_size,
            chunk_size,
            new_head_timeout,
            sync_timeout,
            sync_poll_interval,
            confirmations_after_sync_block,
        } = config;

        let chain_id = http_provider.get_chain_id().await?;
        let topic = topic.into();
        let subscription = ws_provider
            .subscribe_logs(
                &Filter::new()
                    .address(contract_address)
                    .from_block(BlockNumberOrTag::Latest)
                    .event_signature(topic.clone()),
            )
            .channel_size(channel_size.get())
            .await?;

        let ws_stream = stream::unfold(subscription, move |mut rx| async move {
            match rx.recv().await {
                Ok(x) => Some((Ok(x), rx)),
                Err(RecvError::Lagged(_)) => Some((Err(EventStreamError::Lagging), rx)),
                Err(RecvError::Closed) => None,
            }
        });

        // If the chain id is NOT Anvil we don't start at current block unless we manually specify `SkipBackfill::Yes`
        // We almost always start from genesis on Anvil and therefore want to backfill the events that happened before we subscribed.
        if chain_cursor.is_genesis() && chain_id != 31_337 {
            tracing::debug!("chain event cursor is genesis - starting at current block");
            return Ok(ws_stream.boxed());
        } else if skip_backfill == SkipBackfill::Yes {
            tracing::debug!("skipping backfill as requested");
            return Ok(ws_stream.boxed());
        }

        // Use the WS provider to get the current head rather than the HTTP provider's
        // get_block_number — the two providers may not agree on the same block, and
        // using HTTP risks missing blocks. This way we know at least that the node
        // serving the WS has already seen this header.
        //
        // We also take additionally confirmations_after_sync_block headers afterwards. The additional wait time increases the chance that the HTTP and WS provider agree on the block cutoff.
        //
        // Subscribe outside the timeout so we always hold the local id and can
        // unsubscribe afterwards - including when the timeout fires and the
        // inner future is dropped before we can read it.
        let block_subscription = ws_provider.subscribe_blocks().await?;
        let block_sub_id = *block_subscription.local_id();

        let backfill_cutoff = tokio::time::timeout(new_head_timeout, async {
            block_subscription
                .into_stream()
                // +1 to actually collect as many blocks AFTER initial block as confirmations_after_sync_block requests
                .take(confirmations_after_sync_block.get() + 1)
                .map(|h| h.number)
                .collect::<Vec<_>>()
                .await
                .first()
                .copied()
                .ok_or(EventStreamError::CannotFetchHead)
        })
        .await
        .map_err(|_| EventStreamError::CannotFetchHead);

        // Unsubscribe from newHeads
        // we don't want to ramp up RPC provider costs if we cannot unsubscribe
        // therefore we fail
        ws_provider.unsubscribe(block_sub_id).await?;

        let backfill_cutoff = backfill_cutoff??;
        tracing::debug!("backfill cutoff at block: {backfill_cutoff}");

        // now we need to wait until the HTTP provider is also at that specific block number to assure that get_logs will fetch up until the cutoff
        tokio::time::timeout(
            sync_timeout,
            block_until_cutoff(&http_provider, backfill_cutoff, sync_poll_interval),
        )
        .await
        .map_err(|_| EventStreamError::SynchronizingHttpWsTimeout)??;

        let backfill_stream = stream::iter(block_ranges(
            chain_cursor.block,
            backfill_cutoff,
            chunk_size,
        ))
        .then(move |range| {
            fetch_logs(
                contract_address,
                range,
                http_provider.clone(),
                topic.clone(),
                chain_cursor,
            )
        })
        .map_ok(|logs| stream::iter(logs.into_iter().map(Ok)))
        .try_flatten();

        let ws_stream = ws_stream.filter_map(move |log| async move {
            match log {
                Ok(x) => match x.block_number {
                    None => Some(Err(EventStreamError::BlockNumberMissing)),
                    Some(n) if n <= backfill_cutoff => {
                        // drop as already handled by backfill
                        tracing::debug!("skipping event at block {n} - already backfilled");
                        None
                    }
                    Some(_) => Some(Ok(x)),
                },
                Err(e) => Some(Err(e)),
            }
        });

        Ok(backfill_stream.chain(ws_stream).boxed())
    }
}

async fn block_until_cutoff(
    http_provider: &web3::HttpRpcProvider,
    cutoff: u64,
    poll_interval: Duration,
) -> Result<(), EventStreamError> {
    loop {
        let block_number = http_provider.inner().get_block_number().await?;
        if block_number >= cutoff {
            break Ok(());
        }
        tokio::time::sleep(poll_interval).await;
    }
}

fn block_ranges(
    start: u64,
    end: u64,
    chunk_size: NonZeroUsize,
) -> impl Iterator<Item = RangeInclusive<u64>> {
    let chunk_size_u64 = u64::try_from(chunk_size.get()).expect("usize should fit into u64");
    (start..=end)
        .step_by(chunk_size.get())
        .map(move |from| from..=from.saturating_add(chunk_size_u64 - 1).min(end))
}

async fn fetch_logs(
    contract_address: Address,
    range: RangeInclusive<u64>,
    http_provider: web3::HttpRpcProvider,
    event_signature: FilterSet<FixedBytes<32>>,
    chain_cursor: ChainCursor,
) -> Result<Vec<Log>, EventStreamError> {
    tracing::trace!("fetching logs!");
    let filter = Filter::new()
        .address(contract_address)
        .from_block(BlockNumberOrTag::Number(*range.start()))
        .to_block(BlockNumberOrTag::Number(*range.end()))
        .event_signature(event_signature);
    tracing::trace!("get logs for range: {range:?}");

    let logs = http_provider.get_logs(&filter).await?;
    tracing::trace!("got {} logs", logs.len());

    logs.into_iter()
        .filter_map(|log| match filter_block_index(&log, chain_cursor) {
            Ok(true) => Some(Ok(log)),
            Ok(false) => None,
            Err(err) => Some(Err(err)),
        })
        .collect()
}

#[inline]
fn filter_block_index(log: &Log, chain_cursor: ChainCursor) -> Result<bool, EventStreamError> {
    let block_number_log = log
        .block_number
        .ok_or_else(|| EventStreamError::BlockNumberMissing)?;
    let idx_log = log
        .log_index
        .ok_or_else(|| EventStreamError::IndexNumberMissing)?;
    Ok(chain_cursor.is_before(ChainCursor::new(block_number_log, idx_log)))
}

#[cfg(test)]
mod tests {
    use crate::{Environment, web3::HttpRpcProviderBuilder};

    use super::*;
    use std::time::Duration;

    use alloy::{
        network::EthereumWallet,
        node_bindings::Anvil,
        primitives::U256,
        providers::{Provider, ProviderBuilder, WsConnect, ext::AnvilApi},
        signers::local::PrivateKeySigner,
        sol_types::SolEvent as _,
    };

    alloy::sol! {
        #[sol(rpc, bytecode = "6080604052348015600e575f5ffd5b5060b480601a5f395ff3fe6080604052348015600e575f5ffd5b50600436106026575f3560e01c80634d43bec914602a575b5f5ffd5b603960353660046068565b603b565b005b60405181907f1440c4dd67b4344ea1905ec0318995133b550f168b4ee959a0da6b503d7d2414905f90a250565b5f602082840312156077575f5ffd5b503591905056fea2646970667358221220728c746521e437c8e3d44198c6a5d227ed87df09e46ebb8fcb494b485f362a6364736f6c634300081e0033")]
        contract TestEmitter {
            event TestEvent(uint256 indexed value);
            function emitEvent(uint256 value) external;
        }
    }

    const TIMEOUT: Duration = Duration::from_secs(5);

    struct TestHarness {
        _anvil: alloy::node_bindings::AnvilInstance,
        http_provider: web3::HttpRpcProvider,
        ws_provider: DynProvider,
        contract_address: Address,
    }

    impl TestHarness {
        async fn new() -> eyre::Result<Self> {
            Self::with_chain_id(31_337).await
        }

        async fn with_chain_id(chain_id: u64) -> eyre::Result<Self> {
            let anvil = Anvil::new().chain_id(chain_id).spawn();
            let signer: PrivateKeySigner = anvil.keys()[0].clone().into();
            let http_provider =
                HttpRpcProviderBuilder::with_default_values([anvil.endpoint_url()])?
                    .environment(Environment::Dev)
                    .wallet(EthereumWallet::from(signer))
                    .chain_id(chain_id)
                    .build()?;
            let ws_provider = ProviderBuilder::new()
                .connect_ws(WsConnect::new(anvil.ws_endpoint()))
                .await?
                .erased();
            let contract = TestEmitter::deploy(http_provider.inner()).await?;
            ws_provider.anvil_set_interval_mining(2).await?;
            Ok(Self {
                _anvil: anvil,
                http_provider,
                ws_provider,
                contract_address: *contract.address(),
            })
        }

        fn contract(&self) -> TestEmitter::TestEmitterInstance<DynProvider> {
            TestEmitter::new(self.contract_address, self.http_provider.inner())
        }

        async fn emit_event(&self, value: u64) -> eyre::Result<u64> {
            let receipt = self
                .contract()
                .emitEvent(U256::from(value))
                .send()
                .await?
                .get_receipt()
                .await?;
            receipt
                .block_number
                .ok_or_else(|| eyre::eyre!("missing block number on receipt"))
        }

        /// Batch multiple events into a single block.
        /// Returns the block number that contains all the events.
        async fn emit_events_in_one_block(&self, values: &[u64]) -> eyre::Result<u64> {
            // Pause interval mining to prevent it from firing mid-batch
            self.ws_provider.anvil_set_interval_mining(0).await?;
            let mut pending = Vec::new();
            for &v in values {
                let tx = self.contract().emitEvent(U256::from(v)).send().await?;
                pending.push(tx);
            }
            self.ws_provider.anvil_mine(Some(1), None).await?;
            self.ws_provider.anvil_set_interval_mining(2).await?;
            let receipt = pending
                .into_iter()
                .last()
                .expect("At least one receipt there")
                .get_receipt()
                .await?;
            receipt
                .block_number
                .ok_or_else(|| eyre::eyre!("missing block number on receipt"))
        }

        async fn emit_events_in_blocks(
            &self,
            values: &[u64],
            events_per_block: usize,
        ) -> eyre::Result<u64> {
            eyre::ensure!(
                events_per_block > 0,
                "events_per_block must be greater than zero"
            );

            let mut blocks = values.chunks(events_per_block);
            let first_block = self
                .emit_events_in_one_block(
                    blocks
                        .next()
                        .ok_or_else(|| eyre::eyre!("must emit at least one event"))?,
                )
                .await?;
            for block in blocks {
                self.emit_events_in_one_block(block).await?;
            }
            Ok(first_block)
        }

        fn builder(&self, cursor: ChainCursor) -> EventStreamBuilder<Vec<FixedBytes<32>>> {
            EventStreamBuilder::new(
                cursor,
                self.contract_address,
                self.http_provider.clone(),
                self.ws_provider.clone(),
                vec![TestEmitter::TestEvent::SIGNATURE_HASH],
            )
        }
    }

    async fn next_log(
        stream: &mut (impl Stream<Item = Result<Log, EventStreamError>> + Unpin),
    ) -> Log {
        tokio::time::timeout(TIMEOUT, stream.next())
            .await
            .expect("timed out waiting for log")
            .expect("stream ended unexpectedly")
            .expect("stream yielded an error")
    }

    async fn next_log_and_decode(
        stream: &mut (impl Stream<Item = Result<Log, EventStreamError>> + Unpin),
    ) -> U256 {
        decode_log(&next_log(stream).await)
    }

    fn decode_log(log: &Log) -> U256 {
        log.log_decode::<TestEmitter::TestEvent>()
            .expect("Should be able to decode TestEvent")
            .inner
            .data
            .value
    }

    #[tokio::test]
    async fn test_receives_live_events() -> eyre::Result<()> {
        let h = TestHarness::new().await?;
        let mut stream = h.builder(ChainCursor::default()).build().await?;

        h.emit_event(42).await?;

        let log = next_log(&mut stream).await;
        assert_eq!(log.topic0(), Some(&TestEmitter::TestEvent::SIGNATURE_HASH));
        Ok(())
    }

    #[tokio::test]
    async fn test_backfills_historical_events() -> eyre::Result<()> {
        let h = TestHarness::new().await?;

        // Batch 3 events into one block so they get log_index 0, 1, 2.
        // Cursor is at (block - 1, 0), so all three events pass filter_block_index.
        let block = h.emit_events_in_one_block(&[1, 2, 3]).await?;

        let cursor = ChainCursor {
            block: block.saturating_sub(1),
            index: 0,
        };
        let mut stream = h.builder(cursor).build().await?;

        let log1 = next_log_and_decode(&mut stream).await;
        let log2 = next_log_and_decode(&mut stream).await;
        let log3 = next_log_and_decode(&mut stream).await;
        assert_eq!(log1, U256::from(1));
        assert_eq!(log2, U256::from(2));
        assert_eq!(log3, U256::from(3));
        Ok(())
    }

    #[tokio::test]
    async fn test_backfills_historical_events_batch_size_one() -> eyre::Result<()> {
        let h = TestHarness::new().await?;

        let expected = (1..=100).collect::<Vec<_>>();
        let block = h.emit_events_in_blocks(&expected, 5).await?;

        let cursor = ChainCursor {
            block: block.saturating_sub(1),
            index: 0,
        };
        let mut stream = h
            .builder(cursor)
            .chunk_size(NonZeroUsize::try_from(1).expect("1 is non-zero"))
            .build()
            .await?;

        let backfilled = tokio::time::timeout(
            TIMEOUT,
            stream
                .by_ref()
                .take(expected.len())
                .map_ok(|log| decode_log(&log).to::<u64>())
                .try_collect::<Vec<_>>(),
        )
        .await
        .expect("timed out waiting for backfilled logs")?;
        assert_eq!(backfilled, expected);
        Ok(())
    }

    #[tokio::test]
    async fn test_backfills_historical_events_index_cursor() -> eyre::Result<()> {
        let h = TestHarness::new().await?;

        // Batch 3 events into one block so they get log_index 0, 1, 2.
        // Cursor is at (block0, 1), so only the event at log_index 2 passes
        // from this block; all events in the next block also pass.
        let block0 = h.emit_events_in_one_block(&[1, 2, 3]).await?;
        h.emit_events_in_one_block(&[4, 5]).await?;

        let cursor = ChainCursor {
            block: block0,
            index: 1,
        };
        let mut stream = h.builder(cursor).build().await?;

        // log1 must be skipped due to cursor
        // log2 must be skipped due to cursor
        let log3 = next_log_and_decode(&mut stream).await;
        let log4 = next_log_and_decode(&mut stream).await;
        let log5 = next_log_and_decode(&mut stream).await;
        assert_eq!(log3, U256::from(3));
        assert_eq!(log4, U256::from(4));
        assert_eq!(log5, U256::from(5));
        Ok(())
    }

    #[tokio::test]
    async fn test_return_lagging() -> eyre::Result<()> {
        let h = TestHarness::new().await?;

        let cursor = ChainCursor::new(0, 1);
        let mut stream = h
            .builder(cursor)
            .channel_size(NonZeroUsize::try_from(1).expect("1 is non-zero"))
            .build()
            .await?;

        // Batch 3 events into one block. The channel capacity is 1, so
        // receiving 3 events at once guarantees the broadcast channel lags.
        h.emit_events_in_one_block(&[1, 2, 3]).await?;

        let error = tokio::time::timeout(
            TIMEOUT,
            stream
                .by_ref()
                .take(3)
                .map_ok(|log| decode_log(&log).to::<u64>())
                .try_collect::<Vec<_>>(),
        )
        .await
        .expect("timed out waiting for logs")
        .expect_err("should be lagging behind");

        assert!(matches!(error, EventStreamError::Lagging));
        Ok(())
    }

    #[tokio::test]
    async fn test_skip_backfill_ignores_history() -> eyre::Result<()> {
        let h = TestHarness::new().await?;

        let block = h.emit_event(1).await?;
        h.emit_event(2).await?;

        let cursor = ChainCursor {
            block: block.saturating_sub(1),
            index: 0,
        };
        let mut stream = h
            .builder(cursor)
            .skip_backfill(SkipBackfill::Yes)
            .build()
            .await?;

        h.emit_event(99).await?;

        let log = next_log_and_decode(&mut stream).await;
        assert_eq!(log, U256::from(99));
        Ok(())
    }

    #[tokio::test]
    async fn test_backfill_then_live() -> eyre::Result<()> {
        let h = TestHarness::new().await?;

        // Batch 2 historical events into one block (log_index 0, 1).
        // Cursor is at (block - 1, 0), so both events are backfilled.
        let block = h.emit_events_in_one_block(&[1, 2]).await?;

        let cursor = ChainCursor {
            block: block.saturating_sub(1),
            index: 0,
        };
        let mut stream = h.builder(cursor).build().await?;

        h.emit_event(3).await?;

        let backfilled0 = next_log_and_decode(&mut stream).await;
        let backfilled1 = next_log_and_decode(&mut stream).await;
        let live = next_log_and_decode(&mut stream).await;
        assert_eq!(backfilled0, U256::from(1));
        assert_eq!(backfilled1, U256::from(2));
        assert_eq!(live, U256::from(3));
        Ok(())
    }

    #[test]
    fn test_block_ranges_chunk_size_one() {
        let ranges: Vec<_> =
            block_ranges(0, 3, NonZeroUsize::new(1).expect("1 is non-zero")).collect();
        assert_eq!(ranges, vec![0..=0, 1..=1, 2..=2, 3..=3]);
    }

    #[test]
    fn test_block_ranges_chunk_larger_than_range() {
        let ranges: Vec<_> =
            block_ranges(5, 8, NonZeroUsize::new(100).expect("100 is non-zero")).collect();
        assert_eq!(ranges, vec![5..=8]);
    }

    #[test]
    fn test_block_ranges_exact_multiple() {
        let ranges: Vec<_> =
            block_ranges(0, 3, NonZeroUsize::new(2).expect("2 is non-zero")).collect();
        assert_eq!(ranges, vec![0..=1, 2..=3]);
    }

    #[test]
    fn test_block_ranges_start_greater_than_end() {
        let ranges: Vec<_> =
            block_ranges(10, 5, NonZeroUsize::new(3).expect("3 is non-zero")).collect();
        assert!(ranges.is_empty());
    }

    #[tokio::test]
    async fn test_genesis_cursor_starts_from_now() -> eyre::Result<()> {
        // For non-Anvil chains we start at the current block if the cursor is at genesis.
        let h = TestHarness::with_chain_id(1).await?;

        h.emit_event(1).await?;
        h.emit_event(2).await?;

        let mut stream = h.builder(ChainCursor::default()).build().await?;

        h.emit_event(99).await?;

        let decoded = next_log_and_decode(&mut stream).await;
        assert_eq!(decoded, U256::from(99));
        Ok(())
    }

    #[test]
    fn test_block_ranges_single_block() {
        let ranges: Vec<_> =
            block_ranges(5, 5, NonZeroUsize::new(10).expect("10 is non-zero")).collect();
        assert_eq!(ranges, vec![5..=5]);
    }

    #[tokio::test]
    async fn test_new_head_timeout_returns_error() -> eyre::Result<()> {
        let h = TestHarness::new().await?;

        // Stop all mining so subscribe_blocks() never yields a new header
        h.ws_provider.anvil_set_auto_mine(false).await?;
        h.ws_provider.anvil_set_interval_mining(0).await?;

        let cursor = ChainCursor::new(1, 0);
        let result = h
            .builder(cursor)
            .new_head_timeout(Duration::from_millis(100))
            .build()
            .await;

        assert!(matches!(result, Err(EventStreamError::CannotFetchHead)));
        Ok(())
    }

    #[tokio::test]
    async fn test_sync_timeout_returns_error() -> eyre::Result<()> {
        // Anvil A: WS provider, mines normally
        let anvil_ws = Anvil::new().spawn();
        let ws_provider = ProviderBuilder::new()
            .connect_ws(WsConnect::new(anvil_ws.ws_endpoint()))
            .await?
            .erased();
        ws_provider.anvil_set_interval_mining(2).await?;

        // Anvil B: HTTP provider, frozen at genesis
        let anvil_http = Anvil::new().spawn();
        let http_freeze = ProviderBuilder::new()
            .connect_http(anvil_http.endpoint_url())
            .erased();
        http_freeze.anvil_set_auto_mine(false).await?;
        http_freeze.anvil_set_interval_mining(0).await?;

        let signer: PrivateKeySigner = anvil_http.keys()[0].clone().into();
        let http_provider =
            HttpRpcProviderBuilder::with_default_values([anvil_http.endpoint_url()])?
                .environment(Environment::Dev)
                .wallet(EthereumWallet::from(signer))
                .chain_id(31_337)
                .build()?;

        // Mine a block on Anvil A so subscribe_blocks yields a header
        ws_provider.anvil_mine(Some(1), None).await?;

        let cursor = ChainCursor::new(1, 0);
        let result = EventStreamBuilder::new(
            cursor,
            Address::ZERO,
            http_provider,
            ws_provider,
            vec![TestEmitter::TestEvent::SIGNATURE_HASH],
        )
        .sync_timeout(Duration::from_millis(200))
        .sync_poll_interval(Duration::from_millis(50))
        .build()
        .await;

        assert!(matches!(
            result,
            Err(EventStreamError::SynchronizingHttpWsTimeout)
        ));
        Ok(())
    }
}
