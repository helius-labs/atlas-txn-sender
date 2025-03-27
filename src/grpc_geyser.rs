use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use std::{collections::HashMap, sync::Arc, time::Duration};

use cadence_macros::statsd_count;
use dashmap::DashMap;
use futures::sink::SinkExt;
use futures::StreamExt;
use rand::distributions::Alphanumeric;
use rand::Rng;
use solana_sdk::clock::UnixTimestamp;
use solana_sdk::signature::Signature;
use tokio::time::sleep;
use tonic::async_trait;
use tracing::error;
use yellowstone_grpc_client::GeyserGrpcClient;
use yellowstone_grpc_proto::geyser::SubscribeRequestFilterBlocks;
use yellowstone_grpc_proto::geyser::{
    subscribe_update::UpdateOneof, CommitmentLevel, SubscribeRequest, SubscribeRequestFilterSlots,
    SubscribeRequestPing,
};

use crate::solana_rpc::SolanaRpc;

pub struct GrpcGeyserImpl {
    endpoint: String,
    auth_header: Option<String>,
    cur_slot: Arc<AtomicU64>,
    signature_cache: Arc<DashMap<String, (UnixTimestamp, Instant)>>,
}

impl GrpcGeyserImpl {
    pub fn new(endpoint: String, auth_header: Option<String>) -> Self {
        let grpc_geyser = Self {
            endpoint,
            auth_header,
            cur_slot: Arc::new(AtomicU64::new(0)),
            signature_cache: Arc::new(DashMap::new()),
        };
        // polling with processed commitment to get latest leaders
        grpc_geyser.poll_slots();
        // polling with confirmed commitment to get confirmed transactions
        grpc_geyser.poll_blocks();
        grpc_geyser.clean_signature_cache();
        grpc_geyser
    }

    fn clean_signature_cache(&self) {
        let signature_cache = self.signature_cache.clone();
        tokio::spawn(async move {
            loop {
                let signature_cache = signature_cache.clone();
                signature_cache.retain(|_, (_, v)| v.elapsed().as_secs() < 90);
                sleep(Duration::from_secs(60)).await;
            }
        });
    }

    fn poll_blocks(&self) {
        let endpoint = self.endpoint.clone();
        let auth_header = self.auth_header.clone();
        let signature_cache = self.signature_cache.clone();
        tokio::spawn(async move {
            loop {
                let mut grpc_tx;
                let mut grpc_rx;
                {
                    let grpc_client = GeyserGrpcClient::connect::<String, String>(
                        endpoint.clone(),
                        auth_header.clone(),
                        None,
                    );
                    if let Err(e) = grpc_client {
                        error!("Error connecting to gRPC, waiting one second then retrying connect: {}", e);
                        statsd_count!("grpc_connect_error", 1);
                        sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                    let subscription = grpc_client
                        .unwrap()
                        .subscribe_with_request(Some(get_block_subscribe_request()))
                        .await;
                    if let Err(e) = subscription {
                        error!("Error subscribing to gRPC stream, waiting one second then retrying connect: {}", e);
                        statsd_count!("grpc_subscribe_error", 1);
                        sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                    (grpc_tx, grpc_rx) = subscription.unwrap();
                }
                while let Some(message) = grpc_rx.next().await {
                    match message {
                        Ok(message) => match message.update_oneof {
                            Some(UpdateOneof::Block(block)) => {
                                let block_time = block.block_time.unwrap().timestamp;
                                for transaction in block.transactions {
                                    let signature =
                                        Signature::new(&transaction.signature).to_string();
                                    signature_cache.insert(signature, (block_time, Instant::now()));
                                }
                            }
                            Some(UpdateOneof::Ping(_)) => {
                                // This is necessary to keep load balancers that expect client pings alive. If your load balancer doesn't
                                // require periodic client pings then this is unnecessary
                                let ping = grpc_tx.send(ping()).await;
                                if let Err(e) = ping {
                                    error!("Error sending ping: {}", e);
                                    statsd_count!("grpc_ping_error", 1);
                                    break;
                                }
                            }
                            Some(UpdateOneof::Pong(_)) => {}
                            _ => {
                                error!("Unknown message: {:?}", message);
                            }
                        },
                        Err(error) => {
                            error!(
                                "error in block subscribe, resubscribing in 1 second: {error:?}"
                            );
                            statsd_count!("grpc_resubscribe", 1);
                            break;
                        }
                    }
                }
                sleep(Duration::from_secs(1)).await;
            }
        });
    }

    fn poll_slots(&self) {
        let endpoint = self.endpoint.clone();
        let auth_header = self.auth_header.clone();
        let cur_slot = self.cur_slot.clone();
        // let grpc_tx = self.grpc_tx.clone();
        tokio::spawn(async move {
            loop {
                let mut grpc_tx;
                let mut grpc_rx;
                {
                    let grpc_client = GeyserGrpcClient::connect::<String, String>(
                        endpoint.clone(),
                        auth_header.clone(),
                        None,
                    );
                    if let Err(e) = grpc_client {
                        error!("Error connecting to gRPC, waiting one second then retrying connect: {}", e);
                        statsd_count!("grpc_connect_error", 1);
                        sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                    let subscription = grpc_client.unwrap().subscribe().await;
                    if let Err(e) = subscription {
                        error!("Error subscribing to gRPC stream, waiting one second then retrying connect: {}", e);
                        statsd_count!("grpc_subscribe_error", 1);
                        sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                    (grpc_tx, grpc_rx) = subscription.unwrap();
                }
                grpc_tx.send(get_slot_subscribe_request()).await.unwrap();
                while let Some(message) = grpc_rx.next().await {
                    match message {
                        Ok(msg) => {
                            match msg.update_oneof {
                                Some(UpdateOneof::Slot(slot)) => {
                                    cur_slot.store(slot.slot, Ordering::Relaxed);
                                }
                                Some(UpdateOneof::Ping(_)) => {
                                    // This is necessary to keep load balancers that expect client pings alive. If your load balancer doesn't
                                    // require periodic client pings then this is unnecessary
                                    let ping = grpc_tx.send(ping()).await;
                                    if let Err(e) = ping {
                                        error!("Error sending ping: {}", e);
                                        statsd_count!("grpc_ping_error", 1);
                                        break;
                                    }
                                }
                                Some(UpdateOneof::Pong(_)) => {}
                                _ => {
                                    error!("Unknown message: {:?}", msg);
                                }
                            }
                        }
                        Err(error) => {
                            error!("error in slot subscribe, resubscribing in 1 second: {error:?}");
                            statsd_count!("grpc_resubscribe", 1);
                            break;
                        }
                    }
                }
                sleep(Duration::from_secs(1)).await;
            }
        });
    }
}

#[async_trait]
impl SolanaRpc for GrpcGeyserImpl {
    async fn confirm_transaction(&self, signature: String) -> Option<UnixTimestamp> {
        let start = Instant::now();
        // in practice if a tx doesn't land in less than 60 seconds it's probably not going to land
        while start.elapsed() < Duration::from_secs(60) {
            if let Some(block_time) = self.signature_cache.get(&signature) {
                return Some(block_time.0.clone());
            }
            sleep(Duration::from_millis(10)).await;
        }
        return None;
    }
    fn get_next_slot(&self) -> Option<u64> {
        let cur_slot = self.cur_slot.load(Ordering::Relaxed);
        if cur_slot == 0 {
            return None;
        }
        Some(cur_slot)
    }
}

fn generate_random_string(len: usize) -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(len)
        .map(char::from)
        .collect()
}

fn get_block_subscribe_request() -> SubscribeRequest {
    SubscribeRequest {
        blocks: HashMap::from_iter(vec![(
            generate_random_string(20),
            SubscribeRequestFilterBlocks {
                account_include: vec![],
                include_transactions: Some(true),
                include_accounts: Some(false),
                include_entries: Some(false),
            },
        )]),
        commitment: Some(CommitmentLevel::Confirmed.into()),
        ..Default::default()
    }
}

fn get_slot_subscribe_request() -> SubscribeRequest {
    SubscribeRequest {
        slots: HashMap::from_iter(vec![(
            generate_random_string(20).to_string(),
            SubscribeRequestFilterSlots {
                filter_by_commitment: Some(true),
            },
        )]),
        ..Default::default()
    }
}

fn ping() -> SubscribeRequest {
    SubscribeRequest {
        ping: Some(SubscribeRequestPing { id: 1 }),
        ..Default::default()
    }
}
