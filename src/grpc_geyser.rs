use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use cadence_macros::{statsd_count, statsd_gauge};
use crossbeam::channel::{Receiver, Sender};
use dashmap::DashMap;
use futures::sink::SinkExt;
use futures::StreamExt;
use jsonrpsee::core::error;
use solana_sdk::{signature::Signature, slot_history::Slot};
use tokio::{sync::RwLock, time::sleep};
use tracing::{debug, error, info};
use yellowstone_grpc_client::GeyserGrpcClient;
use yellowstone_grpc_proto::{
    geyser::{
        subscribe_update::UpdateOneof, SubscribeRequest, SubscribeRequestFilterSlots,
        SubscribeRequestFilterTransactions, SubscribeRequestPing,
    },
    tonic::service::Interceptor,
};

use crate::{solana_rpc::SolanaRpc, transaction_store::TransactionStore};

pub struct GrpcGeyserImpl<T> {
    grpc_client: Arc<RwLock<GeyserGrpcClient<T>>>,
    transaction_store: Arc<dyn TransactionStore>,
    outbound_slot_rx: Receiver<Slot>,
    outbound_slot_tx: Sender<Slot>,
    // inbound_signature_rx: Receiver<String>,
    // inbound_signature_tx: Sender<String>,
    // outbound_signature_rx: Receiver<String>,
    // outbound_signature_tx: Sender<String>,
    current_signatures: Arc<DashMap<String, Instant>>,
}

impl<T: Interceptor + Send + Sync + 'static> GrpcGeyserImpl<T> {
    pub fn new(
        grpc_client: Arc<RwLock<GeyserGrpcClient<T>>>,
        transaction_store: Arc<dyn TransactionStore>,
    ) -> Self {
        let (outbound_slot_tx, outbound_slot_rx) = crossbeam::channel::unbounded();
        // let (inbound_signature_tx, inbound_signature_rx) = crossbeam::channel::unbounded();
        // let (outbound_signature_tx, outbound_signature_rx) = crossbeam::channel::unbounded();
        let grpc_geyser = Self {
            grpc_client,
            transaction_store,
            outbound_slot_rx,
            outbound_slot_tx,
            // inbound_signature_rx,
            // inbound_signature_tx,
            // outbound_signature_rx,
            // outbound_signature_tx,
            current_signatures: Arc::new(DashMap::new()),
        };
        grpc_geyser.poll_subscription();
        grpc_geyser
    }

    fn poll_subscription(&self) {
        let grpc_client = self.grpc_client.clone();
        let transaction_store = self.transaction_store.clone();
        let outbound_slot_tx = self.outbound_slot_tx.clone();
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
                    grpc_tx
                        .send(get_subscribe_request(transaction_store.get_signatures()))
                        .await
                        .unwrap();
                }
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
                                Some(UpdateOneof::Transaction(tx)) => {
                                    if let Some(transaction) = tx.transaction {
                                        match String::from_utf8(transaction.signature) {
                                            Ok(signature) => {
                                                transaction_store.remove_transaction(signature)
                                            }
                                            Err(e) => {
                                                error!("Error decoding signature: {}", e);
                                                statsd_count!("signature_decode_error", 1);
                                            }
                                        }
                                    } else {
                                        error!("Transaction update without transaction");
                                    }
                                    continue;
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

impl<T: Send + Sync> SolanaRpc for GrpcGeyserImpl<T> {
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

fn get_subscribe_request(signatures: Vec<String>) -> SubscribeRequest {
    let transaction_subscriptions: Vec<(String, SubscribeRequestFilterTransactions)> = signatures
        .into_iter()
        .map(|signature| {
            (
                "txn_sub".to_string(),
                SubscribeRequestFilterTransactions {
                    vote: Some(false),
                    failed: Some(true),
                    signature: Some(signature),
                    account_include: vec![],
                    account_exclude: vec![],
                    account_required: vec![],
                },
            )
        })
        .collect();
    SubscribeRequest {
        slots: HashMap::from_iter(vec![(
            "slot_sub".to_string(),
            SubscribeRequestFilterSlots {
                filter_by_commitment: Some(true),
            },
        )]),
        transactions: HashMap::from_iter(transaction_subscriptions),
        ..Default::default()
    }
}

fn ping() -> SubscribeRequest {
    SubscribeRequest {
        ping: Some(SubscribeRequestPing { id: 1 }),
        ..Default::default()
    }
}
