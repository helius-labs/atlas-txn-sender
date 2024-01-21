use std::{
    hash::{Hash, Hasher},
    sync::Arc,
    time::{Duration, Instant},
};

use cadence_macros::{statsd_count, statsd_gauge, statsd_time};
use dashmap::DashMap;
use solana_sdk::transaction::{self, VersionedTransaction};
use tokio::time::sleep;
use tracing::error;

pub struct TransactionData {
    pub wire_transaction: Vec<u8>,
    pub versioned_transaction: VersionedTransaction,
    pub sent_at: Instant,
}

pub trait TransactionStore: Send + Sync {
    fn add_transaction(&self, transaction: TransactionData);
    fn get_signatures(&self) -> Vec<String>;
    fn remove_transaction(&self, signature: String);
    fn get_wire_transactions(&self) -> Vec<Vec<u8>>;
}

pub struct TransactionStoreImpl {
    transactions: Arc<DashMap<String, TransactionData>>,
}

impl TransactionStoreImpl {
    pub fn new() -> Self {
        let transaction_store = Self {
            transactions: Arc::new(DashMap::new()),
        };
        transaction_store.clean_signatures();
        transaction_store
    }
    fn clean_signatures(&self) {
        let transactions = self.transactions.clone();
        tokio::spawn(async move {
            loop {
                let mut signatures_to_remove = vec![];
                for transaction in transactions.iter() {
                    if transaction.sent_at.elapsed().as_secs() > 90 {
                        signatures_to_remove.push(get_signature(&transaction).unwrap());
                    }
                }
                let transactions_not_landed = signatures_to_remove.len();
                for signature in signatures_to_remove {
                    transactions.remove(&signature.to_string());
                }
                statsd_count!("transactions_not_landed", transactions_not_landed as i64);
                statsd_gauge!("transaction_store_size", transactions.len() as u64);
                sleep(Duration::from_secs(60)).await;
            }
        });
    }
}

impl TransactionStore for TransactionStoreImpl {
    fn add_transaction(&self, transaction: TransactionData) {
        let start = Instant::now();
        if let Some(signature) = get_signature(&transaction) {
            self.transactions.insert(signature.to_string(), transaction);
        } else {
            error!("Transaction has no signatures");
        }
        statsd_time!("add_signature_time", start.elapsed());
    }
    fn get_signatures(&self) -> Vec<String> {
        let start = Instant::now();
        let signatures = self
            .transactions
            .iter()
            .map(|t| get_signature(&t).unwrap())
            .collect();
        statsd_time!("get_signatures_time", start.elapsed());
        signatures
    }
    fn remove_transaction(&self, signature: String) {
        let start = Instant::now();
        let removed_txn = self.transactions.remove(&signature);
        statsd_time!("remove_signature_time", start.elapsed());
        if let Some((_, transaction_data)) = removed_txn {
            statsd_count!("transactions_landed", 1);
            statsd_time!("transaction_land_time", transaction_data.sent_at.elapsed());
        }
    }
    fn get_wire_transactions(&self) -> Vec<Vec<u8>> {
        let start = Instant::now();
        let wire_transactions = self
            .transactions
            .iter()
            .map(|t| t.value().wire_transaction.clone())
            .collect();
        statsd_time!("get_wire_transactions_time", start.elapsed());
        wire_transactions
    }
}

fn get_signature(transaction: &TransactionData) -> Option<String> {
    transaction
        .versioned_transaction
        .signatures
        .get(0)
        .map(|s| s.to_string())
}
