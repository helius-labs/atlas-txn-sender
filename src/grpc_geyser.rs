use std::time::Instant;
use std::{collections::HashMap, sync::Arc, time::Duration};

use cadence_macros::statsd_count;
use crossbeam::channel::{Receiver, Sender};
use futures::sink::SinkExt;
use futures::StreamExt;
use solana_sdk::slot_history::Slot;
use tokio::{sync::RwLock, time::sleep};
use tonic::async_trait;
use tracing::{error, info};
use yellowstone_grpc_client::GeyserGrpcClient;
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
}

impl<T: Interceptor + Send + Sync + 'static> GrpcGeyserImpl<T> {
    pub fn new(grpc_client: Arc<RwLock<GeyserGrpcClient<T>>>) -> Self {
        let (outbound_slot_tx, outbound_slot_rx) = crossbeam::channel::unbounded();
        let grpc_geyser = Self {
            grpc_client,
            outbound_slot_rx,
            outbound_slot_tx,
        };
        grpc_geyser.poll_slots();
        grpc_geyser
    }

    fn poll_slots(&self) {
        let grpc_client = self.grpc_client.clone();
        let outbound_slot_tx = self.outbound_slot_tx.clone();
        // let grpc_tx = self.grpc_tx.clone();
        tokio::spawn(async move {
            loop {
                let mut grpc_tx;
                let mut grpc_rx;
                // let grpc_tx_write = grpc_tx.write().await;
                let mut grpc_client = grpc_client.write().await;
                let subscription = grpc_client.subscribe().await;
                drop(grpc_client);
                if let Err(e) = subscription {
                    error!("Error subscribing to gRPC stream, waiting one second then retrying connect: {}", e);
                    statsd_count!("grpc_subscribe_error", 1);
                    sleep(Duration::from_secs(1)).await;
                    continue;
                }
                (grpc_tx, grpc_rx) = subscription.unwrap();
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
        let mut grpc_rx;
        {
            info!("subscribing with: {}", signature);
            let mut grpc_client = self.grpc_client.write().await;
            let subscription = grpc_client
                .subscribe_with_request(Some(get_signature_subscribe_request(signature)))
                .await;
            drop(grpc_client);
            if let Err(e) = subscription {
                error!("Error subscribing to gRPC stream, waiting one second then retrying connect: {}", e);
                statsd_count!("grpc_subscribe_error", 1);
                return false;
            }
            (_, grpc_rx) = subscription.unwrap();
        }
        let start = Instant::now();
        while let Some(message) = grpc_rx.next().await {
            match message {
                Ok(message) => match message.update_oneof {
                    Some(UpdateOneof::Transaction(_)) => {
                        return true;
                    }
                    _ => {}
                },
                Err(error) => {
                    error!("error in txn subscribe: {error:?}");
                    return false;
                }
            }
            if start.elapsed() > Duration::from_secs(90) {
                return false;
            }
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

fn get_signature_subscribe_request(signature: String) -> SubscribeRequest {
    SubscribeRequest {
        transactions: HashMap::from_iter(vec![(
            signature.to_string(),
            SubscribeRequestFilterTransactions {
                vote: Some(false),
                failed: Some(true),
                signature: Some(signature),
                account_include: vec![],
                account_exclude: vec![],
                account_required: vec![],
            },
        )]),
        commitment: Some(CommitmentLevel::Finalized.into()),
        ..Default::default()
    }
}

fn get_slot_subscribe_request() -> SubscribeRequest {
    SubscribeRequest {
        slots: HashMap::from_iter(vec![(
            "slot_sub".to_string(),
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
