use std::{sync::Arc, time::Duration};

use cadence_macros::{statsd_count, statsd_gauge, statsd_time};
use solana_client::{
    connection_cache::ConnectionCache, nonblocking::tpu_connection::TpuConnection,
};
use solana_program_runtime::compute_budget::ComputeBudget;
use solana_sdk::transaction::{self, VersionedTransaction};
use tokio::{
    runtime::{Builder, Runtime},
    time::sleep,
};
use tonic::async_trait;
use tracing::{error, warn};

use crate::{
    leader_tracker::LeaderTracker,
    rpc_server::RequestMetadata,
    solana_rpc::SolanaRpc,
    transaction_store::{get_signature, TransactionData, TransactionStore},
};

#[async_trait]
pub trait TxnSender: Send + Sync {
    fn send_transaction(&self, txn: TransactionData, request_metadata: Option<RequestMetadata>);
}

pub struct TxnSenderImpl {
    leader_tracker: Arc<dyn LeaderTracker>,
    transaction_store: Arc<dyn TransactionStore>,
    connection_cache: Arc<ConnectionCache>,
    solana_rpc: Arc<dyn SolanaRpc>,
    txn_sender_runtime: Arc<Runtime>,
}

impl TxnSenderImpl {
    pub fn new(
        leader_tracker: Arc<dyn LeaderTracker>,
        transaction_store: Arc<dyn TransactionStore>,
        connection_cache: Arc<ConnectionCache>,
        solana_rpc: Arc<dyn SolanaRpc>,
        txn_sender_threads: usize,
    ) -> Self {
        let txn_sender_runtime = Builder::new_multi_thread()
            .worker_threads(txn_sender_threads)
            .enable_all()
            .build()
            .unwrap();
        let txn_sender = Self {
            leader_tracker,
            transaction_store,
            connection_cache,
            solana_rpc,
            txn_sender_runtime: Arc::new(txn_sender_runtime),
        };
        txn_sender.retry_transactions();
        txn_sender
    }

    fn retry_transactions(&self) {
        let leader_tracker = self.leader_tracker.clone();
        let transaction_store = self.transaction_store.clone();
        let connection_cache = self.connection_cache.clone();
        let txn_sender_runtime = self.txn_sender_runtime.clone();
        tokio::spawn(async move {
            loop {
                let mut transactions_reached_max_retries = vec![];
                let transcations = transaction_store.get_transactions();
                let transaction_retry_queue_length = transcations.len();
                let mut wire_transactions = vec![];
                // get wire transactions and push transactions that reached max retries to transactions_reached_max_retries
                for mut transaction_data in transcations.iter_mut() {
                    if transaction_data.retry_count
                        >= transaction_data.max_retries.unwrap_or(usize::MAX)
                    {
                        transactions_reached_max_retries
                            .push(get_signature(&transaction_data).unwrap());
                    } else {
                        transaction_data.retry_count += 1;
                        wire_transactions.push(transaction_data.wire_transaction.clone());
                    }
                }
                // send wire transactions to leaders
                let wire_transactions = Arc::new(wire_transactions).clone();
                for leader in leader_tracker.get_leaders() {
                    if leader.tpu_quic.is_none() {
                        error!("leader {:?} has no tpu_quic", leader);
                        continue;
                    }
                    let wire_transactions = wire_transactions.clone();
                    let connection_cache = connection_cache.clone();
                    txn_sender_runtime.spawn(async move {
                        for i in 0..3 {
                            let conn = connection_cache
                                .get_nonblocking_connection(&leader.tpu_quic.unwrap());
                            if let Err(e) = conn.send_data_batch(&wire_transactions.clone()).await {
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
                // remove transactions that reached max retries
                for signature in transactions_reached_max_retries {
                    statsd_count!("transactions_reached_max_retries", 1);
                    let transaction_data = transaction_store.remove_transaction(signature);
                    if let Some(transaction_data) = transaction_data {
                        let priority_fees =
                            compute_priority_fee(&transaction_data.versioned_transaction)
                                .map_or(false, |fee| fee > 0)
                                .to_string();
                        statsd_count!("transactions_not_landed", 1, "priority_fees" => &priority_fees);
                        statsd_gauge!(
                            "transaction_retry_queue_length",
                            transaction_retry_queue_length as u64
                        );
                    }
                }
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
        if transaction_data.max_retries.unwrap_or(1) > 0 {
            self.transaction_store
                .add_transaction(transaction_data.clone());
        }
        let priority_fees = compute_priority_fee(&transaction_data.versioned_transaction)
            .map_or(false, |fee| fee > 0)
            .to_string();
        let solana_rpc = self.solana_rpc.clone();
        let transaction_store = self.transaction_store.clone();
        self.txn_sender_runtime.spawn(async move {
            let confirmed_at = solana_rpc.confirm_transaction(signature.clone()).await;
            transaction_store.remove_transaction(signature);
            if let Some(confirmed_at) = confirmed_at {
                statsd_count!("transactions_landed", 1, "priority_fees" => &priority_fees);
                statsd_time!("transaction_land_time", sent_at.elapsed(), "priority_fees" => &priority_fees);
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
    }
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

#[async_trait]
impl TxnSender for TxnSenderImpl {
    fn send_transaction(
        &self,
        transaction_data: TransactionData,
        request_metadata: Option<RequestMetadata>,
    ) {
        self.track_transaction(&transaction_data);
        let api_key = request_metadata
            .map(|m| m.api_key)
            .unwrap_or("none".to_string());
        let mut leader_num = 0;
        for leader in self.leader_tracker.get_leaders() {
            if leader.tpu_quic.is_none() {
                error!("leader {:?} has no tpu_quic", leader);
                continue;
            }
            let connection_cache = self.connection_cache.clone();
            let wire_transaction = transaction_data.wire_transaction.clone();
            let api_key = api_key.clone();
            self.txn_sender_runtime.spawn(async move {
                for i in 0..3 {
                    let conn =
                        connection_cache.get_nonblocking_connection(&leader.tpu_quic.unwrap());
                    if let Err(e) = conn.send_data(&wire_transaction).await {
                        if i == 2 {
                            error!(
                                api_key = api_key,
                                "Failed to send transaction to {:?}: {}", leader, e
                            );
                        } else {
                            warn!(
                                api_key = api_key,
                                "Retrying to send transaction to {:?}: {}", leader, e
                            );
                        }
                    } else {
                        let leader_num_str = &leader_num.to_string();
                        statsd_time!(
                            "transaction_received_by_leader",
                            transaction_data.sent_at.elapsed(), "api_key" => &api_key, "leader_num" => &leader_num_str);
                        return;
                    }
                }
            });
            leader_num += 1;
        }
    }
}
