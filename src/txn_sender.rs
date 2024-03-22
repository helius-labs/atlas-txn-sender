use cadence_macros::{statsd_count, statsd_gauge, statsd_time};
use solana_client::{
    connection_cache::ConnectionCache, nonblocking::tpu_connection::TpuConnection,
};
use solana_program_runtime::{compute_budget::ComputeBudget, prioritization_fee::PrioritizationFeeDetails};
use solana_sdk::transaction::VersionedTransaction;
use std::{sync::Arc, time::Duration};
use tokio::{
    runtime::{Builder, Runtime},
    time::sleep,
};
use tonic::async_trait;
use tracing::{error, warn};

use crate::{
    leader_tracker::LeaderTracker,
    solana_rpc::SolanaRpc,
    transaction_store::{get_signature, TransactionData, TransactionStore},
    utils::round_to_nearest_million,
};
use solana_program_runtime::compute_budget::DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT;
use solana_sdk::borsh0_10::try_from_slice_unchecked;
use solana_sdk::compute_budget::ComputeBudgetInstruction;

#[async_trait]
pub trait TxnSender: Send + Sync {
    fn send_transaction(&self, txn: TransactionData);
}

pub struct TxnSenderImpl {
    leader_tracker: Arc<dyn LeaderTracker>,
    transaction_store: Arc<dyn TransactionStore>,
    connection_cache: Arc<ConnectionCache>,
    solana_rpc: Arc<dyn SolanaRpc>,
    txn_sender_runtime: Arc<Runtime>,
    txn_send_retry_interval_seconds: usize,
}

impl TxnSenderImpl {
    pub fn new(
        leader_tracker: Arc<dyn LeaderTracker>,
        transaction_store: Arc<dyn TransactionStore>,
        connection_cache: Arc<ConnectionCache>,
        solana_rpc: Arc<dyn SolanaRpc>,
        txn_sender_threads: usize,
        txn_send_retry_interval_seconds: usize,
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
            txn_send_retry_interval_seconds,
        };
        txn_sender.retry_transactions();
        txn_sender
    }

    fn retry_transactions(&self) {
        let leader_tracker = self.leader_tracker.clone();
        let transaction_store = self.transaction_store.clone();
        let connection_cache = self.connection_cache.clone();
        let txn_sender_runtime = self.txn_sender_runtime.clone();
        let txn_send_retry_interval_seconds = self.txn_send_retry_interval_seconds.clone();
        tokio::spawn(async move {
            loop {
                let mut transactions_reached_max_retries = vec![];
                let transcations = transaction_store.get_transactions();
                let transaction_retry_queue_length = transcations.len();
                let mut wire_transactions = vec![];
                // get wire transactions and push transactions that reached max retries to transactions_reached_max_retries
                for mut transaction_data in transcations.iter_mut() {
                    if transaction_data.retry_count >= transaction_data.max_retries {
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
                                statsd_count!("transaction_send_error", 1);
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
                        let landed = "false";
                        let PriorityDetails { fee, cu_limit, priority }  = compute_priority_details(&transaction_data.versioned_transaction);
                        let api_key = transaction_data
                            .request_metadata
                            .map(|m| m.api_key)
                            .unwrap_or("none".to_string());
                        let priority_fees_enabled = (fee > 0).to_string();

                        // Collect metrics
                        statsd_count!("transactions_not_landed", 1, "api_key" => &api_key, "priority_fees_enabled" => &priority_fees_enabled);
                        statsd_gauge!("transaction_priority_fee", priority, "landed" => &landed);
                        statsd_gauge!("transaction_compute_limit", cu_limit as u64, "landed" => &landed);
                        statsd_gauge!("transaction_fees", fee, "landed" => &landed);
                        statsd_gauge!(
                            "transaction_retry_queue_length",
                            transaction_retry_queue_length as u64
                        );
                    }
                }
                sleep(Duration::from_secs(txn_send_retry_interval_seconds as u64)).await;
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
        if transaction_data.max_retries > 0 {
            self.transaction_store
                .add_transaction(transaction_data.clone());
        }
        let PriorityDetails { fee, cu_limit, priority }  = compute_priority_details(&transaction_data.versioned_transaction);
        let priority_fees_enabled = (fee > 0).to_string();
        let solana_rpc = self.solana_rpc.clone();
        let transaction_store = self.transaction_store.clone();
        let api_key = transaction_data
            .request_metadata
            .clone()
            .map(|m| m.api_key.clone())
            .unwrap_or("none".to_string());
        self.txn_sender_runtime.spawn(async move {
            let confirmed_at = solana_rpc.confirm_transaction(signature.clone()).await;
            transaction_store.remove_transaction(signature);

            // Collect metrics
            let landed = if let Some(confirmed_at) = confirmed_at {
                statsd_count!("transactions_landed", 1, "api_key" => &api_key, "priority_fees_enabled" => &priority_fees_enabled);
                statsd_time!("transaction_land_time", sent_at.elapsed(), "api_key" => &api_key, "priority_fees_enabled" => &priority_fees_enabled);
                "true"
            } else {
                statsd_count!("transactions_not_landed", 1, "api_key" => &api_key, "priority_fees_enabled" => &priority_fees_enabled);
                "false"
            };
            statsd_gauge!("transaction_priority", priority, "landed" => &landed);
            statsd_gauge!("transaction_compute_limit", cu_limit as u64, "landed" => &landed);
            statsd_gauge!("transaction_priority_fee", fee, "landed" => &landed);
        });
    }
}

pub struct PriorityDetails {
    pub fee: u64,
    pub cu_limit: u32,
    pub priority: u64,
}

pub fn compute_priority_details(transaction: &VersionedTransaction) -> PriorityDetails {
    let mut cu_limit = DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT;
    let mut compute_budget = ComputeBudget::default();
    if let Err(e) = transaction.sanitize() {
        return PriorityDetails {
            fee: 0,
            priority: 0,
            cu_limit,
        };
    }
    let instructions = transaction.message.instructions().iter().map(|ix| {
        match try_from_slice_unchecked(&ix.data) {
            Ok(ComputeBudgetInstruction::SetComputeUnitLimit(compute_unit_limit)) => {
                cu_limit = compute_unit_limit;
            }
            _ => {}
        }
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
        Ok(compute_budget) => PriorityDetails {
            fee: compute_budget.get_fee(),
            priority: compute_budget.get_priority(),
            cu_limit,
        },
        Err(e) => PriorityDetails {
            fee: 0,
            priority: 0,
            cu_limit,
        },
    }
}

#[async_trait]
impl TxnSender for TxnSenderImpl {
    fn send_transaction(&self, transaction_data: TransactionData) {
        self.track_transaction(&transaction_data);
        let api_key = transaction_data
            .request_metadata
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
