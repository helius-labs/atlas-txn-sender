use std::{
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    str::FromStr,
    sync::Arc,
};

use cadence_macros::statsd_count;
use futures::future::join_all;
use solana_client::{
    connection_cache::ConnectionCache, nonblocking::tpu_connection::TpuConnection,
};
use solana_rpc_client_api::response::RpcContactInfo;
use tonic::async_trait;
use tracing::error;

use crate::{
    solana_rpc::SolanaRpc,
    transaction_store::{TransactionData, TransactionStore},
};

#[async_trait]
pub trait TxnSender: Send + Sync {
    async fn send_transaction(&self, leaders: Vec<RpcContactInfo>, txn: TransactionData);
}

pub struct TxnSenderImpl {
    transaction_store: Arc<dyn TransactionStore>,
    connection_cache: Arc<ConnectionCache>,
}

impl TxnSenderImpl {
    pub fn new(
        transaction_store: Arc<dyn TransactionStore>,
        connection_cache: Arc<ConnectionCache>,
    ) -> Self {
        Self {
            transaction_store,
            connection_cache,
        }
    }

    fn retry_transactions(&self) {
        let transaction_store = self.transaction_store.clone();
        let connection_cache = self.connection_cache.clone();
        tokio::spawn(async move {
            loop {
                
            }
        });
    }
}

#[async_trait]
impl TxnSender for TxnSenderImpl {
    async fn send_transaction(&self, leaders: Vec<RpcContactInfo>, transaction: TransactionData) {
        for leader in leaders {
            let connection_cache = self.connection_cache.clone();
            let wire_transaction = transaction.wire_transaction.clone();
            if leader.tpu_quic.is_none() {
                error!("leader {:?} has no tpu_quic", leader);
                continue;
            }
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
