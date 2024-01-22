use tonic::async_trait;

#[async_trait]
pub trait SolanaRpc: Send + Sync {
    fn get_next_slot(&self) -> Option<u64>;
    // return true if signature is confirmed, false otherwise
    async fn confirm_transaction(&self, signatures: String) -> bool;
}
