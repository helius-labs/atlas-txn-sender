use std::{sync::Arc, time::Instant};

use cadence_macros::{statsd_count, statsd_time};
use dashmap::DashMap;
use solana_sdk::transaction::VersionedTransaction;
use tracing::error;

#[derive(Clone, Debug)]
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
        transaction_store
    }
}

impl TransactionStore for TransactionStoreImpl {
    fn add_transaction(&self, transaction: TransactionData) {
        let start = Instant::now();
        if let Some(signature) = get_signature(&transaction) {
            if self.transactions.contains_key(&signature) {
                return;
            }
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
        let _ = self.transactions.remove(&signature);
        statsd_time!("remove_signature_time", start.elapsed());
    }
    fn get_wire_transactions<'a>(&self) -> Vec<Vec<u8>> {
        let start = Instant::now();
        let wire_transactions: Vec<Vec<u8>>;
        let max_queue = 40000;
        if self.transactions.len() < max_queue {
            wire_transactions = self
                .transactions
                .iter()
                .map(|t| t.value().wire_transaction.clone())
                .collect();
        } else {
            // crude mechanism for now for speed to prevent host pain
            wire_transactions = self
                .transactions
                .iter()
                .filter(|v| v.sent_at.elapsed().as_secs() < 10)
                .take(max_queue)
                .map(|t| t.value().wire_transaction.clone())
                .collect();
            statsd_count!(
                "transactions_dropped",
                (self.transactions.len() - wire_transactions.len()) as i64
            );
        }
        statsd_time!("get_wire_transactions_time", start.elapsed());
        wire_transactions
    }
}

pub fn get_signature(transaction: &TransactionData) -> Option<String> {
    transaction
        .versioned_transaction
        .signatures
        .get(0)
        .map(|s| s.to_string())
}
