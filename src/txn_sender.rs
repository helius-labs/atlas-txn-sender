use std::{sync::Arc, time::Duration};

use cadence_macros::{statsd_count, statsd_gauge, statsd_time};
use solana_client::{
    connection_cache::ConnectionCache, nonblocking::tpu_connection::TpuConnection,
};
use solana_program_runtime::compute_budget::ComputeBudget;
use solana_sdk::transaction::VersionedTransaction;
use tokio::time::sleep;
use tonic::async_trait;
use tracing::{error, warn};

use crate::{
    leader_tracker::LeaderTracker,
    solana_rpc::SolanaRpc,
    transaction_store::{get_signature, TransactionData, TransactionStore},
    utils::unix_to_time,
};

#[async_trait]
pub trait TxnSender: Send + Sync {
    async fn send_transaction(&self, txn: TransactionData);
}

pub struct TxnSenderImpl {
    leader_tracker: Arc<dyn LeaderTracker>,
    transaction_store: Arc<dyn TransactionStore>,
    connection_manager: Arc<ConnectionCache>,
    solana_rpc: Arc<dyn SolanaRpc>,
}

impl TxnSenderImpl {
    pub fn new(
        leader_tracker: Arc<dyn LeaderTracker>,
        transaction_store: Arc<dyn TransactionStore>,
        connection_manager: Arc<ConnectionCache>,
        solana_rpc: Arc<dyn SolanaRpc>,
    ) -> Self {
        let txn_sender = Self {
            leader_tracker,
            transaction_store,
            connection_manager,
            solana_rpc,
        };
        txn_sender.retry_transactions();
        txn_sender
    }

    fn retry_transactions(&self) {
        let leader_tracker = self.leader_tracker.clone();
        let transaction_store = self.transaction_store.clone();
        let connection_manager = self.connection_manager.clone();
        tokio::spawn(async move {
            loop {
                let wire_transactions = Arc::new(transaction_store.get_wire_transactions());
                let num_retries = wire_transactions.len();
                for leader in leader_tracker.get_leaders() {
                    if leader.tpu_quic.is_none() {
                        error!("leader {:?} has no tpu_quic", leader);
                        continue;
                    }
                    let connection_manager = connection_manager.clone();
                    let wire_transactions = wire_transactions.clone();
                    tokio::spawn(async move {
                        for i in 0..3 {
                            let conn = connection_manager
                                .get_nonblocking_connection(&leader.tpu_quic.unwrap());
                            if let Err(e) = conn.send_data_batch(&wire_transactions).await {
                                if i == 2 {
                                    error!(
                                        "Failed to send transaction batch to {:?}: {}",
                                        leader, e
                                    );
                                } else {
                                    warn!(
                                        "Retrying to send transaction batch to {:?}: {}",
                                        leader, e
                                    );
                                }
                            } else {
                                return;
                            }
                        }
                    });
                }
                statsd_gauge!("transaction_retry_queue_length", num_retries as u64);
                sleep(Duration::from_secs(1)).await;
            }
        });
    }
    fn track_transaction(&self, transaction_data: &TransactionData) {
        let sent_at = transaction_data.sent_at.clone();
        let signature = get_signature(transaction_data);
        if signature.is_none() {
            return;
        }
        let signature = signature.unwrap();
        self.transaction_store
            .add_transaction(transaction_data.clone());
        let priority_fees = compute_priority_fee(&transaction_data.versioned_transaction)
            .map_or(false, |fee| fee > 0)
            .to_string();
        let solana_rpc = self.solana_rpc.clone();
        let transaction_store = self.transaction_store.clone();
        tokio::spawn(async move {
            let confirmed_at = solana_rpc.confirm_transaction(signature.clone()).await;
            transaction_store.remove_transaction(signature);
            if let Some(confirmed_at) = confirmed_at {
                statsd_count!("transactions_landed", 1, "priority_fees" => &priority_fees);
                statsd_time!("transaction_land_time", sent_at.elapsed().as_secs() as u64);
                // This code doesn't behave as expected, it returns very low times and sometimes negative times, maybe the txns land extremely fast, but it seems fishy.
                // match unix_to_time(confirmed_at).duration_since(sent_at_unix) {
                //     Ok(land_time) => {
                //         statsd_time!("transaction_land_time", land_time.as_secs() as u64);
                //     }
                //     Err(e) => {
                //         error!("Error computing land time: {}", e);
                //     }
                // }
            } else {
                statsd_count!("transactions_not_landed", 1, "priority_fees" => &priority_fees);
            }
        });
    }
}

pub fn compute_priority_fee(transaction: &VersionedTransaction) -> Option<u64> {
    let mut compute_budget = ComputeBudget::default();
    if let Err(e) = transaction.sanitize() {
        return None;
    } else {
        let instructions = transaction.message.instructions().iter().map(|ix| {
            (
                transaction
                    .message
                    .static_account_keys()
                    .get(usize::from(ix.program_id_index))
                    .expect("program id index is sanitized"),
                ix,
            )
        });
        let compute_budget = compute_budget.process_instructions(instructions, true, true);
        match compute_budget {
            Ok(compute_budget) => {
                return Some(compute_budget.get_priority());
            }
            Err(e) => None,
        }
    }
}

#[async_trait]
impl TxnSender for TxnSenderImpl {
    async fn send_transaction(&self, transaction_data: TransactionData) {
        self.track_transaction(&transaction_data);
        for leader in self.leader_tracker.get_leaders() {
            if leader.tpu_quic.is_none() {
                error!("leader {:?} has no tpu_quic", leader);
                continue;
            }
            let connection_manager = self.connection_manager.clone();
            let wire_transaction = transaction_data.wire_transaction.clone();
            tokio::spawn(async move {
                for i in 0..3 {
                    let conn =
                        connection_manager.get_nonblocking_connection(&leader.tpu_quic.unwrap());
                    if let Err(e) = conn.send_data(&wire_transaction).await {
                        if i == 2 {
                            error!("Failed to send transaction to {:?}: {}", leader, e);
                        } else {
                            warn!("Retrying to send transaction to {:?}: {}", leader, e);
                        }
                    } else {
                        return;
                    }
                }
            });
        }
    }
}
