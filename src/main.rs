mod errors;
mod grpc_geyser;
mod leader_tracker;
mod rpc_server;
mod solana_rpc;
mod transaction_store;
mod txn_sender;
mod vendor;

use std::{
    env,
    net::{IpAddr, Ipv4Addr, UdpSocket},
    sync::Arc,
};

use cadence::{BufferedUdpMetricSink, QueuingMetricSink, StatsdClient};
use cadence_macros::set_global_default;
use figment::{providers::Env, Figment};
use grpc_geyser::GrpcGeyserImpl;
use jsonrpsee::server::{middleware::ProxyGetRequestLayer, ServerBuilder};
use leader_tracker::LeaderTrackerImpl;
use rpc_server::{AtlasTxnSenderImpl, AtlasTxnSenderServer};
use serde::Deserialize;
use solana_client::{connection_cache::ConnectionCache, rpc_client::RpcClient};
use solana_sdk::signature::{read_keypair_file, Keypair};
use tracing::{error, info};
use transaction_store::TransactionStoreImpl;
use txn_sender::TxnSenderImpl;

#[derive(Debug, Deserialize)]
struct AtlasTxnSenderEnv {
    identity_keypair_file: Option<String>,
    grpc_url: Option<String>,
    rpc_url: Option<String>,
    port: Option<u16>,
    tpu_connection_pool_size: Option<usize>,
    x_token: Option<String>,
    num_leaders: Option<usize>,
    leader_offset: Option<i64>,
    txn_sender_threads: Option<usize>,
    max_txn_send_retries: Option<usize>,
    txn_send_retry_interval: Option<usize>,
    max_retry_queue_size: Option<usize>,
}

// Defualt on RPC is 4
pub const DEFAULT_TPU_CONNECTION_POOL_SIZE: usize = 4;

#[cfg(not(target_env = "msvc"))]
use tikv_jemallocator::Jemalloc;
#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Init metrics/logging
    let env: AtlasTxnSenderEnv = Figment::from(Env::raw()).extract().unwrap();
    let env_filter = env::var("RUST_LOG")
        .or::<Result<String, ()>>(Ok("info".to_string()))
        .unwrap();
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .json()
        .init();
    new_metrics_client();

    let service_builder = tower::ServiceBuilder::new()
        // Proxy `GET /health` requests to internal `health` method.
        .layer(ProxyGetRequestLayer::new("/health", "health")?);
    let port = env.port.unwrap_or(4040);

    let server = ServerBuilder::default()
        .set_middleware(service_builder)
        .max_request_body_size(15_000_000)
        .max_connections(1_000_000)
        .build(format!("0.0.0.0:{}", port))
        .await
        .unwrap();
    let tpu_connection_pool_size = env
        .tpu_connection_pool_size
        .unwrap_or(DEFAULT_TPU_CONNECTION_POOL_SIZE);
    let connection_cache;
    if let Some(identity_keypair_file) = env.identity_keypair_file.clone() {
        let identity_keypair =
            read_keypair_file(identity_keypair_file).expect("keypair file must exist");
        connection_cache = Arc::new(ConnectionCache::new_with_client_options(
            "atlas-txn-sender",
            tpu_connection_pool_size,
            None, // created if none specified
            Some((&identity_keypair, IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)))),
            None, // not used as far as I can tell
        ));
    } else {
        let identity_keypair = Keypair::new();
        connection_cache = Arc::new(ConnectionCache::new_with_client_options(
            "atlas-txn-sender",
            tpu_connection_pool_size,
            None, // created if none specified
            Some((&identity_keypair, IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)))),
            None, // not used as far as I can tell
        ));
    }

    let transaction_store = Arc::new(TransactionStoreImpl::new());
    let solana_rpc = Arc::new(GrpcGeyserImpl::new(
        env.grpc_url.clone().unwrap(),
        env.x_token.clone(),
    ));
    let rpc_client = Arc::new(RpcClient::new(env.rpc_url.unwrap()));
    let num_leaders = env.num_leaders.unwrap_or(2);
    let leader_offset = env.leader_offset.unwrap_or(0);
    let leader_tracker = Arc::new(LeaderTrackerImpl::new(
        rpc_client,
        solana_rpc.clone(),
        num_leaders,
        leader_offset,
    ));
    let txn_send_retry_interval_seconds = env.txn_send_retry_interval.unwrap_or(2);
    let txn_sender = Arc::new(TxnSenderImpl::new(
        leader_tracker,
        transaction_store.clone(),
        connection_cache,
        solana_rpc,
        env.txn_sender_threads.unwrap_or(4),
        txn_send_retry_interval_seconds,
        env.max_retry_queue_size,
    ));
    let max_txn_send_retries = env.max_txn_send_retries.unwrap_or(5);
    let atlas_txn_sender =
        AtlasTxnSenderImpl::new(txn_sender, transaction_store, max_txn_send_retries);
    let handle = server.start(atlas_txn_sender.into_rpc());
    handle.stopped().await;
    Ok(())
}

fn new_metrics_client() {
    let uri = env::var("METRICS_URI")
        .or::<String>(Ok("127.0.0.1".to_string()))
        .unwrap();
    let port = env::var("METRICS_PORT")
        .or::<String>(Ok("7998".to_string()))
        .unwrap()
        .parse::<u16>()
        .unwrap();
    info!("collecting metrics on: {}:{}", uri, port);
    let socket = UdpSocket::bind("0.0.0.0:0").unwrap();
    socket.set_nonblocking(true).unwrap();

    let host = (uri, port);
    let udp_sink = BufferedUdpMetricSink::from(host, socket).unwrap();
    let queuing_sink = QueuingMetricSink::from(udp_sink);
    let builder = StatsdClient::builder("atlas_txn_sender", queuing_sink);
    let client = builder
        .with_error_handler(|e| error!("statsd metrics error: {}", e))
        .build();
    set_global_default(client);
}
