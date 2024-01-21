pub trait SolanaRpc: Send + Sync {
    fn get_next_slot(&self) -> Option<u64>;
}
