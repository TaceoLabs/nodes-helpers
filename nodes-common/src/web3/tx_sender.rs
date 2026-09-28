//! Transaction sender.
//!
//! Broadcasts transactions to the network strictly one at a time, in
//! submission order, so nonce assignment for a shared account never races.

use std::{num::NonZeroUsize, time::Duration};

use alloy::{
    network::Ethereum,
    providers::{DynProvider, PendingTransactionBuilder, Provider as _},
    rpc::types::TransactionRequest,
};
use serde::Deserialize;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

/// Configuration for [`TxSender`].
#[derive(Debug, Clone, Copy, Deserialize)]
#[non_exhaustive]
pub struct TxSenderConfig {
    /// Maximum time to wait for a single `send_transaction` call to return.
    ///
    /// A hung RPC call would otherwise wedge the sender's single-consumer
    /// queue forever, since it processes one broadcast at a time.
    ///
    /// Defaults to **30 seconds**.
    #[serde(default = "TxSenderConfig::default_send_timeout")]
    #[serde(with = "humantime_serde")]
    pub send_timeout: Duration,

    /// Maximum number of transactions that can be queued for broadcast at
    /// once. `submit` waits once this many are already queued.
    ///
    /// Defaults to **32**.
    #[serde(default = "TxSenderConfig::default_queue_size")]
    pub queue_size: NonZeroUsize,
}

impl TxSenderConfig {
    /// Default `send_timeout`: 30 seconds.
    fn default_send_timeout() -> Duration {
        Duration::from_secs(30)
    }

    /// Default `queue_size`: 32.
    fn default_queue_size() -> NonZeroUsize {
        32.try_into().expect("32 is non-zero")
    }
}

impl Default for TxSenderConfig {
    fn default() -> Self {
        Self {
            send_timeout: Self::default_send_timeout(),
            queue_size: Self::default_queue_size(),
        }
    }
}

/// Errors that can occur when submitting a transaction through
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TxSenderError {
    /// Error when interacting with the contract.
    #[error("contract error: {0}")]
    Contract(#[from] alloy::contract::Error),
    /// `send_transaction` did not return within `TxSenderConfig::send_timeout`.
    #[error("timed out waiting for the RPC to accept the transaction")]
    Timeout,
    /// The background task has stopped.
    #[error("tx sender task has stopped")]
    TaskStopped,
}

/// Broadcasts transactions to the network strictly one at a time, in
/// submission order. Cheaply `Clone`.
#[derive(Clone)]
pub struct TxSender {
    jobs: mpsc::Sender<Job>,
}

struct Job {
    request: TransactionRequest,
    reply: oneshot::Sender<Result<PendingTransactionBuilder<Ethereum>, TxSenderError>>,
}

impl TxSender {
    /// Spawns the background task that broadcasts queued transactions
    /// through `provider`, and returns a handle to submit work to it. The
    /// task stops once `cancellation_token` is cancelled; any request still
    /// queued at that point is dropped without being sent, and `submit`
    /// starts returning `TxSenderError::TaskStopped`.
    ///
    /// The task holds a drop guard on `cancellation_token`, so it cancels
    /// the token when it stops for any reason, including all `TxSender`
    /// handles being dropped — pass a token whose cancellation should mean
    /// "shut down everything sharing it".
    #[must_use]
    pub fn spawn(
        provider: DynProvider,
        config: TxSenderConfig,
        cancellation_token: CancellationToken,
    ) -> (Self, tokio::task::JoinHandle<()>) {
        let (jobs, mut rx) = mpsc::channel::<Job>(config.queue_size.get());
        let task = tokio::spawn(async move {
            let _drop_guard = cancellation_token.drop_guard_ref();
            loop {
                tokio::select! {
                    job = rx.recv() => {
                        let Some(job) = job else {
                            tracing::info!("tx sender channel closed, stopping");
                            break;
                        };
                        // NOTE: if this times out, the job is dropped and the caller gets a timeout error.
                        // The next job in the queue is then processed which may get the same nonce as the timed out job.
                        // If the old jobs still exists in the mempool, the next job will be rejected with a nonce error.
                        // But this is the best we can do without a more complex solution that tracks nonces and retries timed out jobs.
                        let result = match tokio::time::timeout(
                            config.send_timeout,
                            provider.send_transaction(job.request),
                        )
                        .await
                        {
                            Ok(result) => result
                                .map_err(alloy::contract::Error::from)
                                .map_err(TxSenderError::from),
                            Err(_) => Err(TxSenderError::Timeout),
                        };
                        let _res = job.reply.send(result);
                    }
                    () = cancellation_token.cancelled() => {
                        tracing::info!("cancellation received, stopping tx sender");
                        break;
                    }
                }
            }
        });
        (Self { jobs }, task)
    }

    /// Queues `request` for broadcast; resolves once it's this request's
    /// turn and it has been sent, returning the `PendingTransactionBuilder`
    /// to await its receipt with.
    ///
    /// # Errors
    ///
    /// Returns an error if the broadcast itself fails, times out, or if the
    /// background task has stopped.
    pub async fn submit(
        &self,
        request: TransactionRequest,
    ) -> Result<PendingTransactionBuilder<Ethereum>, TxSenderError> {
        let (reply, rx) = oneshot::channel();
        self.jobs
            .send(Job { request, reply })
            .await
            .map_err(|_| TxSenderError::TaskStopped)?;
        rx.await.map_err(|_| TxSenderError::TaskStopped)?
    }
}
