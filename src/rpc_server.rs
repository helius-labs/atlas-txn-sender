use std::{
    fmt::Debug,
    net::{IpAddr, Ipv4Addr},
    str::FromStr,
    sync::Arc,
    time::Instant,
};

use cadence_macros::statsd_time;
use jsonrpsee::{
    core::{async_trait, RpcResult},
    proc_macros::rpc,
    types::{error::INVALID_PARAMS_CODE, ErrorObjectOwned},
};
use solana_client::connection_cache::ConnectionCache;
use solana_rpc_client_api::config::RpcSendTransactionConfig;
use solana_sdk::transaction::VersionedTransaction;
use solana_transaction_status::UiTransactionEncoding;
use tracing::error;

use crate::{
    errors::invalid_request, leader_tracker::LeaderTracker, transaction_store::TransactionData,
    txn_sender::TxnSender, vendor::solana_rpc::decode_and_deserialize,
};

#[rpc(server)]
pub trait AtlasTxnSender {
    #[method(name = "health")]
    async fn health(&self) -> String;
    #[method(name = "sendTransaction")]
    async fn send_transaction(
        &self,
        txn: String,
        params: RpcSendTransactionConfig,
    ) -> RpcResult<String>;
}

pub struct AtlasTxnSenderImpl {
    txn_sender: Arc<dyn TxnSender>,
    leader_tracker: Arc<dyn LeaderTracker>,
}

impl AtlasTxnSenderImpl {
    pub fn new(txn_sender: Arc<dyn TxnSender>, leader_tracker: Arc<dyn LeaderTracker>) -> Self {
        Self {
            txn_sender,
            leader_tracker,
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
    ) -> RpcResult<String> {
        // TODO: Add validation to fail is preflight is true
        // TODO: Add landing time metric
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
        let transaction = TransactionData {
            wire_transaction,
            versioned_transaction,
            sent_at: Instant::now(),
        };
        self.txn_sender
            .send_transaction(transaction)
            .await;
        statsd_time!("send_transaction_time", start.elapsed());
        Ok(signature)
    }
}

fn param<T: FromStr>(param_str: &str, thing: &str) -> Result<T, ErrorObjectOwned> {
    param_str.parse::<T>().map_err(|_e| {
        ErrorObjectOwned::owned(
            INVALID_PARAMS_CODE,
            format!("Invalid Request: Invalid {thing} provided"),
            None::<String>,
        )
    })
}

fn log_error<T: Debug>(metric: &str) -> impl Fn(T) -> T {
    let metric = metric.to_string();
    return move |e: T| -> T {
        error!(metric = metric, "{:?}", e);
        e
    };
}
