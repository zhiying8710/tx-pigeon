use anyhow::Result;
use arti_client::{IsolationToken, StreamPrefs, TorClient, TorClientConfig};
use axum::{
    extract::State,
    http::StatusCode,
    response::Json,
    routing::{get, post},
    Router,
};
use bitcoin::{
    Transaction,
    consensus::{Decodable, Encodable},
    io::Cursor,
    p2p::{
        Address, Magic, ServiceFlags,
        address::AddrV2,
        message::{NetworkMessage, RawNetworkMessage},
        message_blockdata::Inventory,
        message_network::VersionMessage,
    },
};

use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};
use sha3::{Digest, Sha3_256};
use std::{
    collections::HashSet,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::lookup_host,
    sync::{Semaphore, RwLock},
    task::JoinSet,
    time::timeout,
};
use tower_http::cors::CorsLayer;

use data_encoding::BASE32_NOPAD;
use tracing::{error, info};

const DNS_SEEDS: &[&str] = &[
    "dnsseed.bluematt.me",
    "dnsseed.bitcoin.dashjr-list-of-p2p-nodes.us",
    "seed.bitcoinstats.com",
    "seed.btc.petertodd.net",
    "seed.bitcoin.sprovoost.nl",
    "dnsseed.emzy.de",
    "seed.bitcoin.wiz.biz",
    "seed.bitcoin.sipa.be",
    "seed.bitcoin.jonasschnelli.ch",
    "seed.mainnet.achownodes.xyz",
];

const NODE_LIBRE_RELAY: u64 = 1 << 29;
const NODE_NETWORK: u64 = 1 << 0;
const NODE_WITNESS: u64 = 1 << 3;
const MAX_CONCURRENT_DELIVERIES: usize = 100;
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(12);
const NODE_CACHE_DURATION: Duration = Duration::from_secs(300); // 5分钟缓存

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum NetworkAddress {
    Ip(SocketAddr),
    Onion(String),
}

#[derive(Debug, Serialize, Deserialize)]
struct BroadcastRequest {
    tx: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct BroadcastResponse {
    success: bool,
    message: String,
    txid: String,
    success_count: usize,
    total_peers: usize,
}

#[derive(Debug, Serialize, Deserialize)]
struct ErrorResponse {
    error: String,
}

#[derive(Clone)]
struct AppState {
    tor_client: Arc<TorClient<tor_rtcompat::PreferredRuntime>>,
    peer_cache: Arc<RwLock<Option<CachedPeers>>>,
}

#[derive(Debug, Clone)]
struct CachedPeers {
    peers: HashSet<NetworkAddress>,
    last_updated: Instant,
}

impl CachedPeers {
    fn new(peers: HashSet<NetworkAddress>) -> Self {
        Self {
            peers,
            last_updated: Instant::now(),
        }
    }

    fn is_expired(&self) -> bool {
        self.last_updated.elapsed() > NODE_CACHE_DURATION
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_target(false).init();

    info!("Starting tx-pigeon HTTP server...");

    // Initialize Tor client
    info!("Bootstrapping Tor client...");
    let config = TorClientConfig::builder().build()?;
    let tor_client = Arc::new(TorClient::create_bootstrapped(config).await?);

    let state = AppState {
        tor_client,
        peer_cache: Arc::new(RwLock::new(None)),
    };

    // Configure CORS
    let cors = CorsLayer::permissive();

    // Build our application with a route
    let app = Router::new()
        .route("/health", get(health_check))
        .route("/broadcast", post(broadcast_transaction))
        .route("/cache/clear", post(clear_cache))
        .layer(cors)
        .with_state(state);

    // Run it
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await?;
    info!("tx-pigeon HTTP server listening on http://0.0.0.0:3000");
    
    axum::serve(listener, app).await?;

    Ok(())
}

#[axum::debug_handler]
async fn health_check() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "status": "healthy",
        "service": "tx-pigeon",
        "version": env!("CARGO_PKG_VERSION")
    }))
}

#[axum::debug_handler]
async fn broadcast_transaction(
    State(state): State<AppState>,
    Json(payload): Json<BroadcastRequest>,
) -> Result<Json<BroadcastResponse>, (StatusCode, Json<ErrorResponse>)> {
    let tx_hex_string = payload.tx;
    
    // 并行处理：交易解析和节点发现
    let (tx, libre_peers) = tokio::try_join!(
        async {
            // Parse transaction
            let tx_bytes = hex::decode(&tx_hex_string)
                .map_err(|e| anyhow::anyhow!("Invalid hex string: {}", e))?;
            
            bitcoin::consensus::deserialize::<Transaction>(&tx_bytes)
                .map_err(|e| anyhow::anyhow!("Invalid transaction format: {}", e))
        },
        get_or_discover_libre_peers(&state)
    ).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
    })?;
    
    let txid = tx.compute_txid();
    let txid_hex = txid.to_string();

    info!("Received broadcast request for tx: {}", txid_hex);
    info!("Found {} libre relay peers", libre_peers.len());

    if libre_peers.is_empty() {
        return Ok(Json(BroadcastResponse {
            success: false,
            message: "No libre relay nodes found".to_string(),
            txid: txid_hex,
            success_count: 0,
            total_peers: 0,
        }));
    }

    // Broadcast transaction
    let success_count = broadcast_to_peers(libre_peers.clone(), tx, state.tor_client.clone())
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: format!("Failed to broadcast transaction: {}", e),
                }),
            )
        })?;

    let success = success_count > 0;
    let message = if success {
        format!("Transaction broadcasted to {} libre relay nodes", success_count)
    } else {
        "No libre relay nodes accepted the transaction. TX may already be in a block or is invalid.".to_string()
    };

    Ok(Json(BroadcastResponse {
        success,
        message,
        txid: txid_hex,
        success_count,
        total_peers: libre_peers.len(),
    }))
}

async fn discover_seed_addresses() -> Result<Vec<SocketAddr>> {
    let mut seed_addrs = Vec::new();
    let mut seed_tasks = JoinSet::new();

    for seed_host in DNS_SEEDS {
        info!("fetching addrs from {:?}", seed_host);

        let host = seed_host.to_owned();

        seed_tasks.spawn(async move {
            let lookup = lookup_host(format!("{}:8333", seed_host));

            match timeout(Duration::from_secs(2), lookup).await {
                Ok(Ok(addrs)) => {
                    let addrs: Vec<_> = addrs.collect();
                    Ok((host, addrs))
                }
                Ok(Err(e)) => Err(anyhow::Error::new(e)),
                Err(_) => {
                    error!("Timeout while looking up {}", seed_host);
                    Err(anyhow::anyhow!("Timeout"))
                }
            }
        });
    }

    while let Some(res) = seed_tasks.join_next().await {
        match res {
            Ok(Ok((host, addresses))) => {
                info!("{} returned {} IPs", host, addresses.len());
                seed_addrs.extend(addresses);
            }
            Ok(Err(crawl_error)) => {
                error!("dns seed node error: {crawl_error},");
            }
            Err(join_error) => {
                error!("join error during dns seed: {join_error}");
            }
        }
    }

    seed_addrs.shuffle(&mut rand::rng());
    Ok(seed_addrs)
}

async fn discover_libre_peers(seed_addrs: Vec<SocketAddr>) -> Result<HashSet<NetworkAddress>> {
    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_DELIVERIES));
    let mut libre_peers = HashSet::<NetworkAddress>::new();
    let mut crawl_tasks = JoinSet::new();

    for addr in seed_addrs {
        crawl_tasks.spawn({
            let sem = semaphore.clone();
            async move {
                let _permit = sem.acquire_owned().await?;
                crawl_seed_node(&addr).await
            }
        });
    }

    while let Some(res) = crawl_tasks.join_next().await {
        match res {
            Ok(Ok(addresses)) => {
                libre_peers.extend(addresses);
            }
            Ok(Err(crawl_error)) => {
                error!("crawl seed node error: {}", crawl_error);
            }
            Err(join_error) => {
                error!("join error during crawl: {}", join_error);
            }
        }
    }

    Ok(libre_peers)
}

async fn broadcast_to_peers(
    libre_peers: HashSet<NetworkAddress>,
    tx: Transaction,
    tor_client: Arc<TorClient<tor_rtcompat::PreferredRuntime>>,
) -> Result<usize> {
    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_DELIVERIES));
    let common_token = IsolationToken::no_isolation();
    let mut prefs = StreamPrefs::new();
    prefs.set_isolation(common_token);

    let mut poop_delivery_tasks = JoinSet::new();
    for peer_addr in libre_peers {
        let tx_clone = tx.clone();
        let permit = semaphore.clone().acquire_owned().await?;
        let tor_client = tor_client.clone();
        let peer_addr_cloned = peer_addr.clone();
        let prefs = prefs.clone();
        poop_delivery_tasks.spawn(async move {
            let _permit_guard = permit;
            match deliver_poop_tx(peer_addr_cloned.clone(), tx_clone, tor_client, prefs).await {
                Ok(true) => Ok(peer_addr_cloned.clone()),
                Ok(false) => Err((
                    peer_addr_cloned.clone(),
                    "No Tx confirmation, rejected, or skipped by peer.".to_string(),
                )),
                Err(e) => Err((peer_addr_cloned.clone(), e.to_string())),
            }
        });
    }

    let mut success_count = 0;

    while let Some(res) = poop_delivery_tasks.join_next().await {
        match res {
            Ok(Ok(_)) => {
                success_count += 1;
            }
            Ok(Err((_, _))) => (),
            Err(join_error) => {
                error!("join error during poop delivery: {}", join_error);
                if join_error.is_panic() {
                    error!("a poop delivery task panicked!");
                }
            }
        }
    }

    Ok(success_count)
}

fn build_version_msg() -> VersionMessage {
    VersionMessage {
        version: 70016,
        services: ServiceFlags::from(NODE_NETWORK | NODE_WITNESS | NODE_LIBRE_RELAY),
        timestamp: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64,
        receiver: Address::new(
            &SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
            ServiceFlags::from(NODE_NETWORK | NODE_LIBRE_RELAY),
        ),
        sender: Address::new(
            &SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
            ServiceFlags::from(NODE_NETWORK | NODE_WITNESS | NODE_LIBRE_RELAY),
        ),
        nonce: rand::random::<u64>(),
        user_agent: "/Satoshi:27.0.0/".into(),
        start_height: 897157,
        relay: true,
    }
}

async fn deliver_poop_tx(
    addr: NetworkAddress,
    tx: Transaction,
    tor_client: Arc<TorClient<tor_rtcompat::PreferredRuntime>>,
    prefs: StreamPrefs,
) -> Result<bool> {
    let txid = tx.compute_txid();

    let mut stream = match &addr {
        NetworkAddress::Ip(sa) => {
            let target = (sa.ip().to_string(), sa.port());

            timeout(
                CONNECTION_TIMEOUT,
                tor_client.connect_with_prefs(target, &prefs),
            )
            .await
            .map_err(|_| anyhow::anyhow!("timeout connecting to {}", sa))??
        }

        NetworkAddress::Onion(host) => timeout(
            CONNECTION_TIMEOUT,
            tor_client.connect_with_prefs((host.as_str(), 8333), &prefs),
        )
        .await
        .map_err(|_| anyhow::anyhow!("timeout connecting to {}", host))??,
    };

    if let Err(e) = send_msg(&mut stream, NetworkMessage::Version(build_version_msg())).await {
        return Err(e);
    }

    let (mut rd, mut wr) = stream.split();

    let peer_version_message = match timeout(Duration::from_secs(5), async {
        loop {
            match read_msg(&mut rd).await {
                Ok(raw_msg) => {
                    if let NetworkMessage::Version(version) = raw_msg.payload() {
                        break Ok::<VersionMessage, anyhow::Error>(version.clone());
                    }
                }
                Err(e) => {
                    break Err(e);
                }
            }
        }
    })
    .await
    {
        Ok(Ok(vm)) => vm,
        Ok(Err(e)) => {
            return Err(e);
        }
        Err(_) => {
            return Err(anyhow::anyhow!(
                "Timeout waiting for peer Version from {:?}",
                addr
            ));
        }
    };

    let libre_flag_check = ServiceFlags::from(NODE_LIBRE_RELAY);
    if !peer_version_message.services.has(libre_flag_check) {
        return Ok(false);
    }

    if let Err(e) = send_msg(&mut wr, NetworkMessage::Verack).await {
        return Err(e);
    }

    // Dont wait for verack, just send the tx

    if let Err(e) = send_msg(&mut wr, NetworkMessage::Tx(tx.clone())).await {
        return Err(e);
    }

    if let Err(e) = send_msg(
        &mut wr,
        NetworkMessage::GetData(vec![Inventory::Transaction(txid)]),
    )
    .await
    {
        return Err(e);
    }

    let mut tx_confirmed_by_peer = false;
    loop {
        match timeout(CONNECTION_TIMEOUT, read_msg(&mut rd)).await {
            Ok(Ok(m)) => match m.payload() {
                NetworkMessage::Tx(received_tx) => {
                    if received_tx.compute_txid() == txid {
                        info!(
                            "[CONFIRMED HIT] {:?} (UA: '{}') direct hit confirmed on libre node! poop deliverd",
                            addr, peer_version_message.user_agent
                        );
                        tx_confirmed_by_peer = true;
                        break;
                    }
                }
                NetworkMessage::NotFound(not_found_list) => {
                    if not_found_list.iter().any(|inv| {
                        if let Inventory::Transaction(hash) = inv {
                            *hash == txid
                        } else {
                            false
                        }
                    }) {
                        error!(
                            "[NotFound] {:?} (UA: '{}') Possible LIAR detected! This node might be lying and tricking the code!! (If tx {} is already in a block, this is expected)",
                            addr, peer_version_message.user_agent, txid
                        );
                        break;
                    }
                }
                NetworkMessage::Inv(inv_list) => {
                    if inv_list.iter().any(|inv| {
                        if let Inventory::Transaction(hash) = inv {
                            *hash == txid
                        } else {
                            false
                        }
                    }) {
                        info!(
                            "[CONFIRMED HIT] INV returned TX: direct hit confirmed on libre node! poop deliverd to {:?}! (UA: '{}')",
                            addr, peer_version_message.user_agent
                        );
                        tx_confirmed_by_peer = true;
                        break;
                    }
                }

                _ => {}
            },
            Ok(Err(read_err)) => {
                error!(
                    "Read error from {:?} while awaiting Tx confirmation: {}. Possible LIAR detected!",
                    addr, read_err
                );
                break;
            }
            Err(_) => {
                error!(
                    "Timeout waiting for Tx, Inv, or Reject from {:?} (UA: '{}'). Possible LIAR detected!",
                    addr, peer_version_message.user_agent
                );
                break;
            }
        }
    }
    Ok(tx_confirmed_by_peer)
}

async fn crawl_seed_node(seed: &SocketAddr) -> Result<Vec<NetworkAddress>> {
    let mut found_peers = Vec::new();
    info!("crawling seed {:?}", seed);
    let mut stream = match timeout(
        Duration::from_secs(2),
        tokio::net::TcpStream::connect((seed.ip().to_string(), seed.port())),
    )
    .await
    {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            return Err(e.into());
        }
        Err(_) => {
            return Err(anyhow::anyhow!("Timeout connecting to {}", seed));
        }
    };

    send_msg(&mut stream, NetworkMessage::Version(build_version_msg())).await?;

    let (mut rd, mut wr) = stream.split();

    info!("waiting for addresses from {:?}...", seed);
    loop {
        let msg = match timeout(CONNECTION_TIMEOUT, read_msg(&mut rd)).await {
            Ok(Ok(m)) => m,
            _ => break,
        };
        match msg.payload() {
            NetworkMessage::Version(_) => {
                send_msg(&mut wr, NetworkMessage::SendAddrV2).await?;
                send_msg(&mut wr, NetworkMessage::Verack).await?;
            }
            NetworkMessage::Verack => {
                send_msg(&mut wr, NetworkMessage::GetAddr).await?;
            }
            NetworkMessage::Addr(list) => {
                let flag = ServiceFlags::from(NODE_LIBRE_RELAY);
                found_peers.extend(list.iter().filter_map(|(_, a)| {
                    if !a.services.has(flag) {
                        return None;
                    }

                    if let Ok(addr) = a.socket_addr() {
                        Some(NetworkAddress::Ip(addr))
                    } else {
                        None
                    }
                }));
                break;
            }
            NetworkMessage::AddrV2(list) => {
                let flag = ServiceFlags::from(NODE_LIBRE_RELAY);
                found_peers.extend(list.iter().filter_map(|a| {
                    if !a.services.has(flag) {
                        return None;
                    }
                    match &a.addr {
                        AddrV2::Ipv4(ip) => {
                            Some(NetworkAddress::Ip(SocketAddr::new(IpAddr::V4(*ip), a.port)))
                        }
                        AddrV2::Ipv6(ip) => {
                            Some(NetworkAddress::Ip(SocketAddr::new(IpAddr::V6(*ip), a.port)))
                        }
                        AddrV2::TorV3(key) => {
                            let onion_addr = tor_v3_onion_from_pubkey(key);
                            Some(NetworkAddress::Onion(onion_addr))
                        }
                        _ => None,
                    }
                }));
                break;
            }
            _ => {}
        }
    }

    found_peers.shuffle(&mut rand::rng());

    Ok(found_peers)
}

async fn send_msg<S: AsyncWriteExt + Unpin>(stream: &mut S, msg: NetworkMessage) -> Result<()> {
    let mut buf = Vec::new();
    RawNetworkMessage::new(Magic::BITCOIN, msg).consensus_encode(&mut buf)?;

    stream.write_all(&buf).await?;

    stream.flush().await?;
    Ok(())
}

async fn read_msg<R: AsyncReadExt + Unpin>(r: &mut R) -> Result<RawNetworkMessage> {
    let mut hdr = [0u8; 24];
    r.read_exact(&mut hdr).await?;
    let len = u32::from_le_bytes(hdr[16..20].try_into().unwrap()) as usize;
    let mut payload = vec![0; len];
    r.read_exact(&mut payload).await?;
    Ok(RawNetworkMessage::consensus_decode(&mut Cursor::new(
        [hdr.to_vec(), payload].concat(),
    ))?)
}

fn tor_v3_onion_from_pubkey(pubkey: &[u8; 32]) -> String {
    let mut hasher = Sha3_256::new();
    hasher.update(b".onion checksum");
    hasher.update(pubkey);
    hasher.update([0x03]);
    let checksum = &hasher.finalize()[..2];

    let mut addr_raw = Vec::with_capacity(35);
    addr_raw.extend_from_slice(pubkey);
    addr_raw.extend_from_slice(checksum);
    addr_raw.push(0x03);

    BASE32_NOPAD.encode(&addr_raw).to_lowercase() + ".onion"
}

async fn get_or_discover_libre_peers(state: &AppState) -> Result<HashSet<NetworkAddress>> {
    // 检查缓存
    {
        let cache = state.peer_cache.read().await;
        if let Some(cached) = &*cache {
            if !cached.is_expired() {
                info!("Using cached peers: {}", cached.peers.len());
                return Ok(cached.peers.clone());
            }
        }
    }

    // 缓存过期或不存在，重新发现节点
    info!("Cache expired or empty, discovering new peers...");
    
    let seed_addrs = discover_seed_addresses().await?;
    let libre_peers = discover_libre_peers(seed_addrs).await?;
    
    // 更新缓存
    {
        let mut cache = state.peer_cache.write().await;
        *cache = Some(CachedPeers::new(libre_peers.clone()));
    }
    
    Ok(libre_peers)
}

#[axum::debug_handler]
async fn clear_cache(
    State(state): State<AppState>,
) -> Json<serde_json::Value> {
    let mut cache = state.peer_cache.write().await;
    *cache = None;
    
    Json(serde_json::json!({
        "success": true,
        "message": "Peer cache cleared successfully"
    }))
}
