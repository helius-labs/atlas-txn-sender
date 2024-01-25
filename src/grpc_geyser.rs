use std::time::Instant;
use std::{collections::HashMap, sync::Arc, time::Duration};

use cadence_macros::statsd_count;
use crossbeam::channel::{Receiver, Sender};
use dashmap::{DashMap, DashSet};
use futures::sink::SinkExt;
use futures::StreamExt;
use rand::distributions::Alphanumeric;
use rand::Rng;
use solana_sdk::signature::Signature;
use solana_sdk::slot_history::Slot;
use solana_sdk::transaction;
use tokio::{sync::RwLock, time::sleep};
use tonic::async_trait;
use tracing::{error, info};
use yellowstone_grpc_client::GeyserGrpcClient;
use yellowstone_grpc_proto::geyser::SubscribeRequestFilterBlocks;
use yellowstone_grpc_proto::{
    geyser::{
        subscribe_update::UpdateOneof, CommitmentLevel, SubscribeRequest,
        SubscribeRequestFilterSlots, SubscribeRequestFilterTransactions, SubscribeRequestPing,
    },
    tonic::service::Interceptor,
};

use crate::solana_rpc::SolanaRpc;

pub struct GrpcGeyserImpl<T> {
    grpc_client: Arc<RwLock<GeyserGrpcClient<T>>>,
    outbound_slot_rx: Receiver<Slot>,
    outbound_slot_tx: Sender<Slot>,
    signature_cache: Arc<DashMap<String, Instant>>,
}

impl<T: Interceptor + Send + Sync + 'static> GrpcGeyserImpl<T> {
    pub fn new(grpc_client: Arc<RwLock<GeyserGrpcClient<T>>>) -> Self {
        let (outbound_slot_tx, outbound_slot_rx) = crossbeam::channel::unbounded();
        let grpc_geyser = Self {
            grpc_client,
            outbound_slot_rx,
            outbound_slot_tx,
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
                signature_cache.retain(|_, v| v.elapsed() < Duration::from_secs(90));
                sleep(Duration::from_secs(60)).await;
            }
        });
    }

    fn poll_blocks(&self) {
        let grpc_client = self.grpc_client.clone();
        let signature_cache = self.signature_cache.clone();
        tokio::spawn(async move {
            loop {
                let mut grpc_tx;
                let mut grpc_rx;
                {
                    let mut grpc_client = grpc_client.write().await;
                    let subscription = grpc_client
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
                                for transaction in block.transactions {
                                    let signature =
                                        Signature::new(&transaction.signature).to_string();
                                    signature_cache.insert(signature, Instant::now());
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
                            error!("error in txn subscribe, resubscribing in 1 second: {error:?}");
                            sleep(Duration::from_secs(1)).await;
                        }
                    }
                }
                error!("exiting loop");
            }
        });
    }

    fn poll_slots(&self) {
        let grpc_client = self.grpc_client.clone();
        let outbound_slot_tx = self.outbound_slot_tx.clone();
        // let grpc_tx = self.grpc_tx.clone();
        tokio::spawn(async move {
            loop {
                let mut grpc_tx;
                let mut grpc_rx;
                {
                    let mut grpc_client = grpc_client.write().await;
                    let subscription = grpc_client.subscribe().await;
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
                                    if let Err(e) = outbound_slot_tx.send(slot.slot) {
                                        error!("Error sending slot: {}", e);
                                        statsd_count!("outbound_slot_tx_error", 1);
                                        continue;
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
                                    error!("Unknown message: {:?}", msg);
                                }
                            }
                        }
                        Err(error) => {
                            error!("error: {error:?}");
                            break;
                        }
                    }
                }
                info!("gRPC stream disconnected, reconnecting in one second");
                sleep(Duration::from_secs(1)).await;
            }
        });
    }
}

#[async_trait]
impl<T: Interceptor + Send + Sync> SolanaRpc for GrpcGeyserImpl<T> {
    async fn confirm_transaction(&self, signature: String) -> bool {
        let start = Instant::now();
        // in practice if a tx doesn't land in less than 60 seconds it's probably not going to land
        while start.elapsed() < Duration::from_secs(60) {
            if self.signature_cache.contains_key(&signature) {
                return true;
            }
            sleep(Duration::from_millis(50)).await;
        }
        return false;
    }
    fn get_next_slot(&self) -> Option<u64> {
        match self.outbound_slot_rx.try_recv() {
            Ok(slot) => Some(slot),
            Err(e) => {
                if e == crossbeam::channel::TryRecvError::Empty {
                    None
                } else {
                    error!("Error receiving slot: {}", e);
                    None
                }
            }
        }
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
