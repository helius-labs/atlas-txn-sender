use std::{sync::Arc, time::Instant};

use cadence_macros::{statsd_count, statsd_time};
use jsonrpsee::{
    core::{async_trait, RpcResult},
    proc_macros::rpc,
    types::{error::INVALID_PARAMS_CODE, ErrorObjectOwned},
};
use serde::Deserialize;
use solana_rpc_client_api::config::RpcSendTransactionConfig;
use solana_sdk::transaction::VersionedTransaction;
use solana_transaction_status::UiTransactionEncoding;
use tracing::{error, info, warn};

use crate::{
    errors::invalid_request,
    leader_tracker::LeaderTracker,
    solana_rpc::SolanaRpc,
    transaction_store::{TransactionData, TransactionStore},
    txn_sender::TxnSender,
    vendor::solana_rpc::decode_and_deserialize,
};

// jsonrpsee does not make it easy to access http data,
// so creating this optional param to pass in metadata
#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all(serialize = "camelCase", deserialize = "camelCase"))]
pub struct RequestMetadata {
    pub api_key: String,
}

#[rpc(server)]
pub trait AtlasTxnSender {
    #[method(name = "health")]
    async fn health(&self) -> String;
    #[method(name = "sendTransaction")]
    async fn send_transaction(
        &self,
        txn: String,
        params: RpcSendTransactionConfig,
        request_metadata: Option<RequestMetadata>,
    ) -> RpcResult<String>;
    #[method(name = "sendTransactionBundle")]
    async fn send_transaction_batch_serial(
        &self,
        txns: Vec<String>,
        params: RpcSendTransactionConfig,
        request_metadata: Option<RequestMetadata>,
    ) -> RpcResult<Vec<String>>;
}

pub struct AtlasTxnSenderImpl {
    txn_sender: Arc<dyn TxnSender>,
    transaction_store: Arc<dyn TransactionStore>,
    solana_rpc: Arc<dyn SolanaRpc>,
    leader_tracker: Arc<dyn LeaderTracker>,
    max_txn_send_retries: usize,
}

impl AtlasTxnSenderImpl {
    pub fn new(
        txn_sender: Arc<dyn TxnSender>,
        transaction_store: Arc<dyn TransactionStore>,
        solana_rpc: Arc<dyn SolanaRpc>,
        leader_tracker: Arc<dyn LeaderTracker>,
        max_txn_send_retries: usize,
    ) -> Self {
        Self {
            txn_sender,
            transaction_store,
            solana_rpc,
            leader_tracker,
            max_txn_send_retries,
        }
    }
}

#[async_trait]
impl AtlasTxnSenderServer for AtlasTxnSenderImpl {
    async fn health(&self) -> String {
        "ok".to_string()
    }
    async fn send_transaction(
        &self,
        txn: String,
        params: RpcSendTransactionConfig,
        request_metadata: Option<RequestMetadata>,
    ) -> RpcResult<String> {
        let sent_at = Instant::now();
        let api_key = request_metadata
            .clone()
            .map(|m| m.api_key)
            .unwrap_or("none".to_string());
        statsd_count!("send_transaction", 1, "api_key" => &api_key);
        validate_send_transaction_params(&params)?;
        let start = Instant::now();
        let encoding = params.encoding.unwrap_or(UiTransactionEncoding::Base58);
        let binary_encoding = encoding.into_binary_encoding().ok_or_else(|| {
            invalid_request(&format!(
                "unsupported encoding: {encoding}. Supported encodings: base58, base64"
            ))
        })?;
        let (wire_transaction, versioned_transaction) =
            match decode_and_deserialize::<VersionedTransaction>(txn, binary_encoding) {
                Ok((wire_transaction, versioned_transaction)) => {
                    (wire_transaction, versioned_transaction)
                }
                Err(e) => {
                    return Err(invalid_request(&e.to_string()));
                }
            };
        let signature = versioned_transaction.signatures[0].to_string();
        if self.transaction_store.has_signature(&signature) {
            statsd_count!("duplicate_transaction", 1, "api_key" => &api_key);
            return Ok(signature);
        }
        let transaction = TransactionData {
            wire_transaction,
            versioned_transaction,
            sent_at,
            retry_count: 0,
            max_retries: std::cmp::min(
                self.max_txn_send_retries,
                params.max_retries.unwrap_or(self.max_txn_send_retries),
            ),
            request_metadata,
        };
        self.txn_sender.send_transaction(transaction);
        statsd_time!(
            "send_transaction_time",
            start.elapsed(),
            "api_key" => &api_key
        );
        Ok(signature)
    }
    async fn send_transaction_batch_serial(
        &self,
        txns: Vec<String>,
        params: RpcSendTransactionConfig,
        request_metadata: Option<RequestMetadata>,
    ) -> RpcResult<Vec<String>> {
        let api_key = request_metadata
            .clone()
            .map(|m| m.api_key)
            .unwrap_or("none".to_string());
        let txn_batch_size = txns.len().to_string();
        statsd_count!("send_transaction_batch_serial", 1, "api_key" => &api_key, "batch_size" => &txn_batch_size);
        validate_send_transaction_params(&params)?; // Validate params once for the batch

        let encoding = params.encoding.unwrap_or(UiTransactionEncoding::Base58);
        let binary_encoding = encoding.into_binary_encoding().ok_or_else(|| {
            invalid_request(&format!(
                "unsupported encoding: {encoding}. Supported encodings: base58, base64"
            ))
        })?;

        let mut successful_signatures = Vec::with_capacity(txns.len());
        let mut skipped_signatures: Vec<(String, String)> = Vec::new();

        for (index, txn_string) in txns.into_iter().enumerate() {
            let start_single_txn = Instant::now();
            // 1. Decode Transaction
            let (wire_transaction, versioned_transaction) = match decode_and_deserialize::<
                VersionedTransaction,
            >(
                txn_string.clone(),
                binary_encoding,
            ) {
                Ok((wt, vt)) => (wt, vt),
                Err(e) => {
                    error!("Failed to decode transaction at index {}: {}", index, e);
                    // Return error, indicating which one failed and prior successes
                    return Err(ErrorObjectOwned::owned(
                        INVALID_PARAMS_CODE,
                        format!(
                            "Failed to decode transaction at index {}: {}. Successful signatures so far: {:?}, Skipped signatures: {:?}",
                            index, e, successful_signatures, skipped_signatures
                        ),
                        None::<String>,
                    ));
                }
            };

            let signature = versioned_transaction.signatures[0].to_string();
            let txn_recent_blockhash = versioned_transaction.message.recent_blockhash().to_string();

            let current_slot_opt = self.leader_tracker.get_current_slot();

            let mut should_skip = false;
            let mut skip_reason = "";

            if let Some(current_slot) = current_slot_opt {
                match self
                    .solana_rpc
                    .get_slot_for_blockhash(&txn_recent_blockhash)
                {
                    Some(blockhash_slot) => {
                        if current_slot > blockhash_slot && current_slot - blockhash_slot > 150 {
                            should_skip = true;
                            skip_reason = "blockhash expired (current_slot - blockhash_slot > 150)";
                            warn!(
                                "Skipping transaction {} at index {} due to expired blockhash (BH slot: {}, Current slot: {})",
                                signature, index, blockhash_slot, current_slot
                            );
                        }
                        // else: Blockhash is recent enough or cache is slightly behind, proceed
                    }
                    None => {
                        // Blockhash not found in our cache.
                        should_skip = true;
                        skip_reason = "blockhash not found in cache";
                        warn!("Blockhash {} for tx {} at index {} not found in local cache.", txn_recent_blockhash, signature, index);
                    }
                }
            } else {
                // Current slot not available. Cannot perform check.
                warn!("Current slot not available. Cannot perform blockhash validity check for tx {} at index {}. Proceeding.", signature, index);
            }

            if should_skip {
                skipped_signatures.push((signature.clone(), skip_reason.to_string()));
                continue;
            }

            statsd_count!("send_transaction_batch_serial.txn_processed", 1, "api_key" => &api_key);
            if self.transaction_store.has_signature(&signature) {
                warn!("Transaction {} at index {} already seen in store. Proceeding with confirmation check.", signature, index);
                statsd_count!("send_transaction_batch_serial.duplicate_in_store", 1, "api_key" => &api_key);
            }

            let transaction_data = TransactionData {
                wire_transaction,
                versioned_transaction,
                sent_at: Instant::now(),
                retry_count: 0,
                max_retries: params.max_retries.unwrap_or(0),
                request_metadata: request_metadata.clone(),
            };

            // Send and wait for confirmation
            self.txn_sender.send_transaction(transaction_data.clone());
            let confirmation_result = self.txn_sender.confirm_transaction(&transaction_data).await;


            match confirmation_result {
                Some(confirmation_timestamp) => {
                    info!(
                        "Transaction {} at index {} confirmed serially at timestamp {}.",
                        signature, index, confirmation_timestamp
                    );
                    successful_signatures.push(signature.clone());
                    statsd_time!(
                        "send_transaction_batch_serial.txn_confirmed_latency",
                        start_single_txn.elapsed(),
                        "api_key" => &api_key
                    );
                    statsd_count!("send_transaction_batch_serial.txn_confirmed", 1, "api_key" => &api_key);
                }
                None => {
                    error!(
                        "Transaction {} at index {} failed to confirm within timeout (serial batch).",
                        signature, index
                    );
                    statsd_count!("send_transaction_batch_serial.txn_timeout", 1, "api_key" => &api_key);

                    return Err(ErrorObjectOwned::owned(
                        jsonrpsee::types::error::CALL_EXECUTION_FAILED_CODE,
                        format!(
                            "Transaction at index {} (signature: {}) failed to confirm within timeout. Successful signatures: {:?}, Skipped signatures: {:?}",
                            index, signature, successful_signatures, skipped_signatures
                        ),
                        None::<String>,
                    ));
                }
            }
        }

        // If the loop completes, no txn failed on chain
        info!(
            "Successfully processed batch. {} confirmed, {} skipped. Initial size: {}.",
            successful_signatures.len(),
            skipped_signatures.len(),
            txn_batch_size
        );
        let final_batch_size_tag = successful_signatures.len().to_string();
        let skipped_count_tag = skipped_signatures.len().to_string();
        statsd_count!("send_transaction_batch_serial.batch_success", 1, "api_key" => &api_key, "confirmed_batch_size" => &final_batch_size_tag, "skipped_count" => &skipped_count_tag);

        if !skipped_signatures.is_empty() {
            warn!("Skipped signatures in batch: {:?}", skipped_signatures);
        }
        Ok(successful_signatures)
    }
}

fn validate_send_transaction_params(
    params: &RpcSendTransactionConfig,
) -> Result<(), ErrorObjectOwned> {
    if !params.skip_preflight {
        return Err(invalid_request("running preflight check is not supported"));
    }
    Ok(())
}
