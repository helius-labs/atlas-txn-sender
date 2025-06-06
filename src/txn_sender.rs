use cadence_macros::{statsd_count, statsd_gauge, statsd_time};
use solana_client::{
    connection_cache::ConnectionCache, nonblocking::tpu_connection::TpuConnection,
};
use solana_program_runtime::compute_budget::{ComputeBudget, MAX_COMPUTE_UNIT_LIMIT};
use solana_sdk::{
    commitment_config::CommitmentConfig,
    signature::Signature,
    transaction::{TransactionError, VersionedTransaction},
    transport::TransportError,
};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    runtime::{Builder, Runtime},
    time::{sleep, timeout},
};
use tonic::async_trait;
use tracing::{error, warn};

use crate::{
    leader_tracker::LeaderTracker,
    solana_rpc::SolanaRpc,
    transaction_store::{get_signature, TransactionData, TransactionStore},
};
use solana_program_runtime::compute_budget::DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT;
use solana_sdk::borsh0_10::try_from_slice_unchecked;
use solana_sdk::compute_budget::ComputeBudgetInstruction;

const RETRY_COUNT_BINS: [i32; 6] = [0, 1, 2, 5, 10, 25];
const MAX_RETRIES_BINS: [i32; 5] = [0, 1, 5, 10, 30];
const MAX_TIMEOUT_SEND_DATA: Duration = Duration::from_millis(500);
const MAX_TIMEOUT_SEND_DATA_BATCH: Duration = Duration::from_millis(500);
const SEND_TXN_RETRIES: usize = 10;

#[async_trait]
pub trait TxnSender: Send + Sync {
    fn send_transaction(&self, txn: TransactionData) -> Result<Signature, TransportError>;
    fn send_transaction_bundle(
        &self,
        txn_data_bundle: Vec<TransactionData>,
    ) -> Result<Vec<Signature>, TransportError>;
}

pub struct TxnSenderImpl {
    leader_tracker: Arc<dyn LeaderTracker>,
    transaction_store: Arc<dyn TransactionStore>,
    connection_cache: Arc<ConnectionCache>,
    solana_rpc: Arc<dyn SolanaRpc>,
    txn_sender_runtime: Arc<Runtime>,
    txn_send_retry_interval_seconds: usize,
    max_retry_queue_size: Option<usize>,
}

impl TxnSenderImpl {
    pub fn new(
        leader_tracker: Arc<dyn LeaderTracker>,
        transaction_store: Arc<dyn TransactionStore>,
        connection_cache: Arc<ConnectionCache>,
        solana_rpc: Arc<dyn SolanaRpc>,
        txn_sender_threads: usize,
        txn_send_retry_interval_seconds: usize,
        max_retry_queue_size: Option<usize>,
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
            max_retry_queue_size,
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
        let max_retry_queue_size = self.max_retry_queue_size.clone();
        tokio::spawn(async move {
            loop {
                let mut transactions_reached_max_retries = vec![];
                let transaction_map = transaction_store.get_transactions();
                let queue_length = transaction_map.len();
                statsd_gauge!("transaction_retry_queue_length", queue_length as u64);

                // Shed transactions by retry_count, if necessary.
                if let Some(max_size) = max_retry_queue_size {
                    if queue_length > max_size {
                        warn!(
                            "Transaction retry queue length is over the limit of {}: {}. Load shedding transactions with highest retry count.", 
                            max_size,
                            queue_length
                        );
                        let mut transactions: Vec<(String, TransactionData)> = transaction_map
                            .iter()
                            .map(|x| (x.key().to_owned(), x.value().to_owned()))
                            .collect();
                        transactions.sort_by(|(_, a), (_, b)| a.retry_count.cmp(&b.retry_count));
                        let transactions_to_remove = transactions[(max_size + 1)..].to_vec();
                        for (signature, _) in transactions_to_remove {
                            transaction_store.remove_transaction(signature.clone());
                            transaction_map.remove(&signature);
                        }
                        let records_dropped = queue_length - max_size;
                        statsd_gauge!("transactions_retry_queue_dropped", records_dropped as u64);
                    }
                }

                let mut wire_transactions = vec![];
                for mut transaction_data in transaction_map.iter_mut() {
                    wire_transactions.push(transaction_data.wire_transaction.clone());
                    if transaction_data.retry_count >= transaction_data.max_retries {
                        transactions_reached_max_retries
                            .push(get_signature(&transaction_data).unwrap());
                    } else {
                        transaction_data.retry_count += 1;
                    }
                }
                for wire_transaction in wire_transactions.iter() {
                    let mut leader_num = 0;
                    for leader in leader_tracker.get_leaders() {
                        if leader.tpu_quic.is_none() {
                            error!("leader {:?} has no tpu_quic", leader);
                            continue;
                        }
                        let connection_cache = connection_cache.clone();
                        let sent_at = Instant::now();
                        let leader = Arc::new(leader.clone());
                        let wire_transaction = wire_transaction.clone();
                        txn_sender_runtime.spawn(async move {
                        // retry unless its a timeout
                        for i in 0..SEND_TXN_RETRIES {
                            let conn = connection_cache 
                                .get_nonblocking_connection(&leader.tpu_quic.unwrap());
                            if let Ok(result) = timeout(MAX_TIMEOUT_SEND_DATA_BATCH, conn.send_data(&wire_transaction)).await {
                                if let Err(e) = result {
                                    if i == SEND_TXN_RETRIES-1 {
                                        error!(
                                            retry = "true",
                                            "Failed to send transaction batch to {:?}: {}",
                                            leader, e
                                        );
                                        statsd_count!("transaction_send_error", 1, "retry" => "true", "last_attempt" => "true");
                                    } else {
                                        statsd_count!("transaction_send_error", 1, "retry" => "true", "last_attempt" => "false");
                                    }
                                } else {
                                    let leader_num_str = leader_num.to_string();
                                    statsd_time!(
                                        "transaction_received_by_leader",
                                        sent_at.elapsed(), "leader_num" => &leader_num_str, "api_key" => "not_applicable", "retry" => "true");
                                    return;
                                }
                            } else {
                                // Note: This is far too frequent to log. It will fill the disks on the host and cost too much on DD.
                                statsd_count!("transaction_send_timeout", 1);
                            }
                        }
                    });
                        leader_num += 1;
                    }
                }
                // remove transactions that reached max retries
                for signature in transactions_reached_max_retries {
                    let _ = transaction_store.remove_transaction(signature);
                    statsd_count!("transactions_reached_max_retries", 1);
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
        self.transaction_store
            .add_transaction(transaction_data.clone());
        let PriorityDetails {
            fee,
            cu_limit,
            priority,
        } = compute_priority_details(&transaction_data.versioned_transaction);
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
            let transcation_data = transaction_store.remove_transaction(signature);
            let mut retries = None;
            let mut max_retries = None;
            if let Some(transaction_data) = transcation_data {
                retries = Some(transaction_data.retry_count as i32);
                max_retries = Some(transaction_data.max_retries as i32);
            }

            let retries_tag = bin_counter_to_tag(retries, &RETRY_COUNT_BINS.to_vec());
            let max_retries_tag: String = bin_counter_to_tag(max_retries, &MAX_RETRIES_BINS.to_vec());

            // Collect metrics
            // We separate the retry metrics to reduce the cardinality with API key and price.
            let landed = if let Some(_) = confirmed_at {
                statsd_count!("transactions_landed", 1, "priority_fees_enabled" => &priority_fees_enabled, "retries" => &retries_tag, "max_retries_tag" => &max_retries_tag);
                statsd_count!("transactions_landed_by_key", 1, "api_key" => &api_key);
                statsd_time!("transaction_land_time", sent_at.elapsed(), "api_key" => &api_key, "priority_fees_enabled" => &priority_fees_enabled);
                "true"
            } else {
                statsd_count!("transactions_not_landed", 1, "priority_fees_enabled" => &priority_fees_enabled, "retries" => &retries_tag, "max_retries_tag" => &max_retries_tag);
                statsd_count!("transactions_not_landed_by_key", 1, "api_key" => &api_key);
                statsd_count!("transactions_not_landed_retries", 1, "priority_fees_enabled" => &priority_fees_enabled, "retries" => &retries_tag, "max_retries_tag" => &max_retries_tag);
                "false"
            };
            statsd_time!("transaction_priority", priority, "landed" => &landed);
            statsd_time!("transaction_priority_fee", fee, "landed" => &landed);
            statsd_time!("transaction_compute_limit", cu_limit as u64, "landed" => &landed);
        });
    }

    // Validate if transaction is confirmed/finalized and successful
    fn transaction_confirmed_or_finalized_and_successful(
        &self,
        txn_signer_signature: &Signature,
    ) -> bool {
        let txn_confirmed = self.leader_tracker.transaction_committed_and_successful(
            txn_signer_signature,
            CommitmentConfig::confirmed(),
        );
        let txn_finalized = self.leader_tracker.transaction_committed_and_successful(
            txn_signer_signature,
            CommitmentConfig::finalized(),
        );

        !txn_confirmed && !txn_finalized
    }

    fn preflight_checks(&self, transaction: &VersionedTransaction) -> Result<(), TransactionError> {
        // Verify that all signers have signed the message
        let all_txn_signatures = transaction.verify_with_results();
        for signed in all_txn_signatures {
            if !signed {
                return Err(TransactionError::SignatureFailure);
            }
        }

        // Verify blockhash validity
        let txn_blockhash = transaction.message.recent_blockhash();
        let valid_txn_blockhash_confirmed = self
            .leader_tracker
            .check_blockhash_validity(txn_blockhash, CommitmentConfig::confirmed());
        let valid_finalized_txn_blockhash = self
            .leader_tracker
            .check_blockhash_validity(txn_blockhash, CommitmentConfig::finalized());

        if !valid_txn_blockhash_confirmed && !valid_finalized_txn_blockhash {
            return Err(TransactionError::BlockhashNotFound);
        }

        Ok(())
    }
}

pub struct PriorityDetails {
    pub fee: u64,
    pub cu_limit: u32,
    pub priority: u64,
}

pub fn compute_priority_details(transaction: &VersionedTransaction) -> PriorityDetails {
    let mut cu_limit = DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT;
    let mut compute_budget = ComputeBudget::new(DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT as u64);
    if let Err(_e) = transaction.sanitize() {
        return PriorityDetails {
            fee: 0,
            priority: 0,
            cu_limit,
        };
    }
    let instructions = transaction.message.instructions().iter().map(|ix| {
        match try_from_slice_unchecked(&ix.data) {
            Ok(ComputeBudgetInstruction::SetComputeUnitLimit(compute_unit_limit)) => {
                cu_limit = compute_unit_limit.min(MAX_COMPUTE_UNIT_LIMIT);
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
        Err(_e) => PriorityDetails {
            fee: 0,
            priority: 0,
            cu_limit,
        },
    }
}

#[async_trait]
impl TxnSender for TxnSenderImpl {
    fn send_transaction(
        &self,
        transaction_data: TransactionData,
    ) -> Result<Signature, TransportError> {
        self.track_transaction(&transaction_data);

        // On-chain verification: Validate transaction signature, message integrity and blockhash
        let on_chain_verification = || match transaction_data
            .versioned_transaction
            .verify_and_hash_message()
        {
            Ok(blockhash) => {
                let valid_confirmed_blockhash = self
                    .leader_tracker
                    .check_blockhash_validity(&blockhash, CommitmentConfig::confirmed());
                let valid_finalized_blockhash = self
                    .leader_tracker
                    .check_blockhash_validity(&blockhash, CommitmentConfig::finalized());

                Ok(!valid_confirmed_blockhash && !valid_finalized_blockhash)
            }
            Err(e) => return Err(e),
        };

        let signer_signature = transaction_data
            .versioned_transaction
            .signatures
            .get(0)
            .expect("signer signature is expected to be present");

        let api_key = transaction_data
            .request_metadata
            .map(|m| m.api_key)
            .unwrap_or("none".to_string());
        let mut leader_num = 0;
        let mut transaction_confirmed_or_finalized = false;

        for leader in self.leader_tracker.get_leaders() {
            // Don't send transactions to rest of the leaders if it is confirmed/finalized and successful
            if self.transaction_confirmed_or_finalized_and_successful(signer_signature) {
                transaction_confirmed_or_finalized = true;
                break;
            }

            if !on_chain_verification()? {
                return Err(TransportError::Custom(
                    "Transaction failed on-chain verification".to_string(),
                ));
            }

            if leader.tpu_quic.is_none() {
                error!("leader {:?} has no tpu_quic", leader);
                continue;
            }
            let connection_cache = self.connection_cache.clone();
            let wire_transaction = transaction_data.wire_transaction.clone();

            let api_key = api_key.clone();

            self.txn_sender_runtime.spawn(async move {
                for i in 0..SEND_TXN_RETRIES {
                    let conn =
                        connection_cache.get_nonblocking_connection(&leader.tpu_quic.unwrap());
                    if let Ok(result) = timeout(MAX_TIMEOUT_SEND_DATA, conn.send_data(&wire_transaction)).await {
                            if let Err(e) = result {
                                if i == SEND_TXN_RETRIES-1 {
                                    error!(
                                        retry = "false",
                                        "Failed to send transaction to {:?}: {}",
                                        leader, e
                                    );
                                    statsd_count!("transaction_send_error", 1, "retry" => "false", "last_attempt" => "true");
                                } else {
                                    statsd_count!("transaction_send_error", 1, "retry" => "false", "last_attempt" => "false");
                                }
                        } else {
                            let leader_num_str = leader_num.to_string();
                            statsd_time!(
                            "transaction_received_by_leader",
                            transaction_data.sent_at.elapsed(), "leader_num" => &leader_num_str, "api_key" => &api_key, "retry" => "false");

                            return;
                        }
                    } else {
                        // Note: This is far too frequent to log. It will fill the disks on the host and cost too much on DD.
                        statsd_count!("transaction_send_timeout", 1);
                    }
                }
            });
            leader_num += 1;
        }

        if transaction_confirmed_or_finalized {
            Ok(*signer_signature)
        } else {
            Err(TransportError::Custom(
                "Send Transaction failed".to_string(),
            ))
        }
    }

    fn send_transaction_bundle(
        &self,
        transaction_data_bundle: Vec<TransactionData>,
    ) -> Result<Vec<Signature>, TransportError> {
        let mut successful_txn_signatures: Vec<Signature> = Vec::new();
        let mut prev_transaction: Option<TransactionData> = None;

        for curr_txn_data in transaction_data_bundle {
            if let Some(ref prev_txn) = prev_transaction {
                let prev_txn_signer_signature =
                    if let Some(sign) = prev_txn.versioned_transaction.signatures.get(0) {
                        sign
                    } else {
                        return Err(TransportError::TransactionError(
                            TransactionError::SignatureFailure,
                        ));
                    };

                if !self
                    .transaction_confirmed_or_finalized_and_successful(prev_txn_signer_signature)
                {
                    return Err(TransportError::Custom(
                        "Previous transaction is not confirmed/finalized and successful"
                            .to_string(),
                    ));
                }
            }

            self.preflight_checks(&curr_txn_data.versioned_transaction)?;

            let curr_txn_signature = self.send_transaction(curr_txn_data.clone())?;
            successful_txn_signatures.push(curr_txn_signature);
            prev_transaction = Some(curr_txn_data);
        }

        Ok(successful_txn_signatures)
    }
}

fn bin_counter_to_tag(counter: Option<i32>, bins: &Vec<i32>) -> String {
    if counter.is_none() {
        return "none".to_string();
    }
    let counter = counter.unwrap();

    // Iterate through the bins vector to find the appropriate bin
    let mut bin_start = "-inf".to_string();
    let mut bin_end = "inf".to_string();
    for bin in bins.iter().rev() {
        if counter >= *bin {
            bin_start = bin.to_string();
            break;
        }

        bin_end = bin.to_string();
    }
    format!("{}_{}", bin_start, bin_end)
}

#[test]
fn test_bin_counter() {
    let bins = vec![0, 1, 2, 5, 10, 25];
    assert_eq!(bin_counter_to_tag(None, &bins), "none");
    assert_eq!(bin_counter_to_tag(Some(-100), &bins), "-inf_0");
    assert_eq!(bin_counter_to_tag(Some(0), &bins), "0_1");
    assert_eq!(bin_counter_to_tag(Some(1), &bins), "1_2");
    assert_eq!(bin_counter_to_tag(Some(2), &bins), "2_5");
    assert_eq!(bin_counter_to_tag(Some(3), &bins), "2_5");
    assert_eq!(bin_counter_to_tag(Some(17), &bins), "10_25");
    assert_eq!(bin_counter_to_tag(Some(34), &bins), "25_inf");
}
