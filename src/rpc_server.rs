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
    max_txn_send_retries: usize,
}

impl AtlasTxnSenderImpl {
    pub fn new(
        txn_sender: Arc<dyn TxnSender>,
        transaction_store: Arc<dyn TransactionStore>,
        max_txn_send_retries: usize,
    ) -> Self {
        Self {
            txn_sender,
            max_txn_send_retries,
            transaction_store,
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
        let batch_size = txns.len().to_string();
        statsd_count!("send_transaction_batch_serial", 1, "api_key" => &api_key, "batch_size" => &batch_size);
        validate_send_transaction_params(&params)?; // Validate params once for the batch

        let encoding = params.encoding.unwrap_or(UiTransactionEncoding::Base58);
        let binary_encoding = encoding.into_binary_encoding().ok_or_else(|| {
            invalid_request(&format!(
                "unsupported encoding: {encoding}. Supported encodings: base58, base64"
            ))
        })?;

        let mut successful_signatures = Vec::with_capacity(txns.len());

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
                                "Failed to decode transaction at index {}: {}. Successful signatures so far: {:?}",
                                index, e, successful_signatures
                            ),
                            None::<String>,
                        ));
                }
            };

            let signature = versioned_transaction.signatures[0].to_string();
            statsd_count!("send_transaction_batch_serial.txn_processed", 1, "api_key" => &api_key);
            if self.transaction_store.has_signature(&signature) {
                warn!("Transaction {} at index {} already seen in store, attempting confirmation check.", signature, index);
            }
            let transaction_data = TransactionData {
                wire_transaction,
                versioned_transaction,
                sent_at: Instant::now(),
                retry_count: 0,
                max_retries: params.max_retries.unwrap_or(0),
                request_metadata: request_metadata.clone(),
            };

            self.txn_sender.send_transaction(transaction_data.clone());
            let confirmation_result = self.txn_sender.confirm_transaction(&transaction_data).await;

            match confirmation_result {
                Some(confirmation_timestamp) => {
                    // Success!
                    info!(
                        "Transaction {} at index {} confirmed at timestamp {}.",
                        signature, index, confirmation_timestamp
                    );
                    successful_signatures.push(signature.clone());
                    statsd_time!(
                        "send_transaction_batch_serial.txn_confirmed_latency",
                        start_single_txn.elapsed(),
                        "api_key" => &api_key
                    );
                    statsd_count!("send_transaction_batch_serial.txn_confirmed", 1, "api_key" => &api_key);

                    self.transaction_store.remove_transaction(signature.clone());
                }
                None => {
                    error!(
                        "Transaction {} at index {} failed to confirm within timeout.",
                        signature, index
                    );
                    statsd_count!("send_transaction_batch_serial.txn_timeout", 1, "api_key" => &api_key);

                    self.transaction_store.remove_transaction(signature.clone());
                    return Err(ErrorObjectOwned::owned(
                        jsonrpsee::types::error::CALL_EXECUTION_FAILED_CODE,
                        format!(
                            "Transaction at index {} (signature: {}) failed to confirm within timeout. Successful signatures so far: {:?}",
                            index, signature, successful_signatures
                        ),
                        None::<String>,
                    ));
                }
            }
        }

        // If the loop completes, all transactions were successful
        info!(
            "Successfully processed batch of {} transactions.",
            successful_signatures.len()
        );
        let batch_size = successful_signatures.len().to_string();
        statsd_count!("send_transaction_batch_serial.batch_success", 1, "api_key" => &api_key, "batch_size" => &batch_size);
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
