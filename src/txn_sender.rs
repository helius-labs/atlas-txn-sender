use std::{sync::Arc, time::Duration};

use cadence_macros::{statsd_count, statsd_time};
use solana_client::nonblocking::tpu_connection::TpuConnection;
use tokio::time::sleep;
use tonic::async_trait;
use tracing::error;

use crate::{
    connection_manager::ConnectionManager,
    leader_tracker::LeaderTracker,
    solana_rpc::SolanaRpc,
    transaction_store::{get_signature, TransactionData, TransactionStore},
};

#[async_trait]
pub trait TxnSender: Send + Sync {
    async fn send_transaction(&self, txn: TransactionData);
}

pub struct TxnSenderImpl {
    leader_tracker: Arc<dyn LeaderTracker>,
    transaction_store: Arc<dyn TransactionStore>,
    connection_manager: Arc<dyn ConnectionManager>,
    solana_rpc: Arc<dyn SolanaRpc>,
}

impl TxnSenderImpl {
    pub fn new(
        leader_tracker: Arc<dyn LeaderTracker>,
        transaction_store: Arc<dyn TransactionStore>,
        connection_manager: Arc<dyn ConnectionManager>,
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
                let wire_transactions = transaction_store.get_wire_transactions();
                let num_retries = wire_transactions.len();
                for leader in leader_tracker.get_leaders() {
                    if leader.tpu_quic.is_none() {
                        error!("leader {:?} has no tpu_quic", leader);
                        continue;
                    }
                    let connection_manager = connection_manager.clone();
                    let wire_transactions = wire_transactions.clone();
                    tokio::spawn(async move {
                        let conn = connection_manager
                            .get_nonblocking_connection(&leader.tpu_quic.unwrap());
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
    fn track_transaction(&self, transaction_data: &TransactionData) {
        let sent_at = transaction_data.sent_at.clone();
        let signature = get_signature(transaction_data);
        if signature.is_none() {
            return;
        }
        let signature = signature.unwrap();
        self.transaction_store
            .add_transaction(transaction_data.clone());
        let solana_rpc = self.solana_rpc.clone();
        let transaction_store = self.transaction_store.clone();
        tokio::spawn(async move {
            let confirmed = solana_rpc.confirm_transaction(signature.clone()).await;
            transaction_store.remove_transaction(signature);
            if confirmed {
                statsd_count!("transactions_landed", 1);
                statsd_time!("transaction_land_time", sent_at.elapsed());
            } else {
                statsd_count!("transactions_not_landed", 1);
            }
        });
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
                let conn = connection_manager.get_nonblocking_connection(&leader.tpu_quic.unwrap());
                if let Err(e) = conn.send_data(&wire_transaction).await {
                    error!("failed to send transaction to {:?}: {}", leader, e);
                    statsd_count!("send_transaction_error", 1);
                }
            });
        }
    }
}
