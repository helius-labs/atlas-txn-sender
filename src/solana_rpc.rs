use solana_sdk::clock::UnixTimestamp;
use tonic::async_trait;

#[async_trait]
pub trait SolanaRpc: Send + Sync {
    fn get_next_slot(&self) -> Option<u64>;
    // return block_time if confirmed, None otherwise
    async fn confirm_transaction(&self, signatures: String) -> Option<UnixTimestamp>;
    // fetch recent blockhash
    async fn get_recent_blockhash(&self) -> Option<solana_sdk::hash::Hash>;
}
