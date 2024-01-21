use std::{
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    str::FromStr,
    sync::Arc,
    time::Duration,
};

use cadence_macros::statsd_count;
use futures::future::join_all;
use solana_client::{
    connection_cache::ConnectionCache, nonblocking::tpu_connection::TpuConnection,
};
use solana_rpc_client_api::response::RpcContactInfo;
use tokio::time::sleep;
use tonic::async_trait;
use tracing::error;

use crate::{
    leader_tracker::{self, LeaderTracker},
    transaction_store::{TransactionData, TransactionStore},
};

#[async_trait]
pub trait TxnSender: Send + Sync {
    async fn send_transaction(&self, txn: TransactionData);
}

pub struct TxnSenderImpl {
    leader_tracker: Arc<dyn LeaderTracker>,
    transaction_store: Arc<dyn TransactionStore>,
    connection_cache: Arc<ConnectionCache>,
}

impl TxnSenderImpl {
    pub fn new(
        leader_tracker: Arc<dyn LeaderTracker>,
        transaction_store: Arc<dyn TransactionStore>,
        connection_cache: Arc<ConnectionCache>,
    ) -> Self {
        let txn_sender = Self {
            leader_tracker,
            transaction_store,
            connection_cache,
        };
        txn_sender.retry_transactions();
        txn_sender
    }

    fn retry_transactions(&self) {
        let leader_tracker = self.leader_tracker.clone();
        let transaction_store = self.transaction_store.clone();
        let connection_cache = self.connection_cache.clone();
        tokio::spawn(async move {
            loop {
                let wire_transactions = transaction_store.get_wire_transactions();
                let num_retries = wire_transactions.len();
                for leader in leader_tracker.get_leaders() {
                    if leader.tpu_quic.is_none() {
                        error!("leader {:?} has no tpu_quic", leader);
                        continue;
                    }
                    let connection_cache = connection_cache.clone();
                    let wire_transactions = wire_transactions.clone();
                    tokio::spawn(async move {
                        let conn =
                            connection_cache.get_nonblocking_connection(&leader.tpu_quic.unwrap());
                        if let Err(e) = conn.send_data_batch(&wire_transactions.clone()).await {
                            error!("failed to send transaction batch to {:?}: {}", leader, e);
                            statsd_count!("send_transaction_batch_error", 1);
                        }
                    });
                }
                statsd_count!("send_transaction_retries", num_retries as i64);
                sleep(Duration::from_secs(1)).await;
            }
        });
    }
}

#[async_trait]
impl TxnSender for TxnSenderImpl {
    async fn send_transaction(&self, transaction: TransactionData) {
        for leader in self.leader_tracker.get_leaders() {
            if leader.tpu_quic.is_none() {
                error!("leader {:?} has no tpu_quic", leader);
                continue;
            }
            let connection_cache = self.connection_cache.clone();
            let wire_transaction = transaction.wire_transaction.clone();
            tokio::spawn(async move {
                let conn = connection_cache.get_nonblocking_connection(&leader.tpu_quic.unwrap());
                if let Err(e) = conn.send_data(&wire_transaction).await {
                    error!("failed to send transaction to {:?}: {}", leader, e);
                    statsd_count!("send_transaction_error", 1);
                }
            });
        }
        self.transaction_store.add_transaction(transaction);
    }
}
