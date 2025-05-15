use anyhow::Result;
use bitcoin::{
    Transaction,
    consensus::{Decodable, Encodable},
    io::Cursor,
    p2p::{
        Address, Magic, ServiceFlags,
        message::{NetworkMessage, RawNetworkMessage},
        message_blockdata::Inventory,
        message_network::VersionMessage,
    },
};
use clap::{Parser, arg, command};
use std::{
    collections::HashSet,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpStream, lookup_host},
    sync::Semaphore,
    task::JoinSet,
};
use tracing::{error, info};

const DNS_SEEDS: &[&str] = &[
    "dnsseed.bluematt.me",
    "dnsseed.bitcoin.dashjr.org",
    "seed.bitcoinstats.com",
    "seed.btc.petertodd.org",
    "seed.bitcoin.sprovoost.nl",
    "dnsseed.emzy.de",
    "seed.bitcoin.wiz.biz",
];

const NODE_LIBRE_RELAY: u64 = 1 << 29;
const MAX_CONCURRENT_DELIVERIES: usize = 200;

#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Args {
    /// Hex-encoded raw transaction you’d like to blast
    #[arg(long)]
    tx: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_target(false).init();

    info!("time to blast some nodes with pigeon poop! 🕊️💩");

    let args = Args::parse();
    let tx_hex_string = args.tx;
    let tx = bitcoin::consensus::deserialize::<Transaction>(&hex::decode(tx_hex_string)?)?;

    info!(
        "blasting tx {:?} to libre relay nodes...",
        tx.compute_txid()
    );

    let mut tx_raw_msg_bytes = Vec::new();
    RawNetworkMessage::new(Magic::BITCOIN, NetworkMessage::Tx(tx.clone()))
        .consensus_encode(&mut tx_raw_msg_bytes)?;

    let mut seed_addrs = Vec::new();

    for seed_host in DNS_SEEDS {
        info!("fetching addrs from {:?}", seed_host);
        if let Ok(resolved_addrs) = lookup_host(format!("{}:8333", seed_host)).await {
            seed_addrs.extend(resolved_addrs);
        }
    }

    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_DELIVERIES));

    info!("found {} seed node addresses", seed_addrs.len());

    let mut libre_peers = HashSet::<SocketAddr>::new();
    let mut crawl_tasks = JoinSet::new();
    for addr in seed_addrs {
        let permit = semaphore.clone().acquire_owned().await?;
        crawl_tasks.spawn(async move {
            let res = crawl_seed_node(addr).await;
            drop(permit);
            res
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

    let mut poop_delivery_tasks = JoinSet::new();
    for peer_addr in libre_peers.clone() {
        let tx_clone = tx.clone();
        let permit = semaphore.clone().acquire_owned().await?;
        poop_delivery_tasks.spawn(async move {
            let _permit_guard = permit;
            match deliver_poop_tx(peer_addr, tx_clone).await {
                Ok(true) => Ok(peer_addr),
                Ok(false) => Err((
                    peer_addr,
                    "No Tx confirmation, rejected, or skipped by peer.".to_string(),
                )),
                Err(e) => Err((peer_addr, e.to_string())),
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

    if success_count == 0 {
        error!(
            "No libre relay nodes accepted the transaction. TX {} may already be in a block or its invalid.",
            tx.compute_txid()
        );
        return Ok(());
    }

    info!(
        "TX: {:?} blasted to {} libre relay nodes. GLHF",
        tx.compute_txid(),
        success_count,
    );

    Ok(())
}

fn build_version_msg(addr: SocketAddr) -> VersionMessage {
    VersionMessage {
        version: 70016,
        services: ServiceFlags::from(NODE_LIBRE_RELAY),
        timestamp: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64,
        receiver: Address::new(&addr, ServiceFlags::NONE),
        sender: Address::new(
            &SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
            ServiceFlags::from(NODE_LIBRE_RELAY),
        ),
        nonce: 420,
        user_agent: "/Satoshi:25.0.0/".into(),
        start_height: 1337,
        relay: true,
    }
}

async fn deliver_poop_tx(addr: SocketAddr, tx: Transaction) -> Result<bool> {
    let mut stream =
        match tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(addr)).await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                return Err(e.into());
            }
            Err(_) => {
                return Err(anyhow::anyhow!("Timeout connecting to {}", addr));
            }
        };
    stream.set_nodelay(true)?;

    if let Err(e) = send_msg(
        &mut stream,
        NetworkMessage::Version(build_version_msg(addr)),
    )
    .await
    {
        return Err(e);
    }

    let (mut rd, mut wr) = stream.split();

    let peer_version_message = match tokio::time::timeout(Duration::from_secs(10), async {
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
                "Timeout waiting for peer Version from {}",
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

    match tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match read_msg(&mut rd).await {
                Ok(raw_msg) => {
                    if let NetworkMessage::Verack = raw_msg.payload() {
                        break Ok::<(), anyhow::Error>(());
                    }
                }
                Err(e) => break Err(e),
            }
        }
    })
    .await
    {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => {
            return Err(e);
        }
        Err(_) => {
            return Err(anyhow::anyhow!("Timeout waiting for Verack from {}", addr));
        }
    }

    if let Err(e) = send_msg(&mut wr, NetworkMessage::Tx(tx.clone())).await {
        return Err(e);
    }

    info!(
        "Waiting for confirmation of TX from libre relay node {}",
        addr
    );

    if let Err(e) = send_msg(
        &mut wr,
        NetworkMessage::GetData(vec![Inventory::Transaction(tx.compute_txid())]),
    )
    .await
    {
        return Err(e);
    }

    let mut tx_confirmed_by_peer = false;
    loop {
        match tokio::time::timeout(Duration::from_secs(10), read_msg(&mut rd)).await {
            Ok(Ok(m)) => match m.payload() {
                NetworkMessage::Tx(received_tx) => {
                    if received_tx.compute_txid() == tx.compute_txid() {
                        info!(
                            "[CONFIRMED HIT] GETDATA returned TX: direct hit confirmed on libre node! poop deliverd to {}!",
                            addr
                        );
                        tx_confirmed_by_peer = true;
                        break;
                    }
                }
                NetworkMessage::NotFound(not_found_list) => {
                    if not_found_list.iter().any(|inv| {
                        if let Inventory::Transaction(hash) = inv {
                            *hash == tx.compute_txid()
                        } else {
                            false
                        }
                    }) {
                        error!(
                            "[NotFound] {} (UA: '{}', Services: {:?}) Possible LIAR detected! (If tx {} is already in a block, this is expected)",
                            addr,
                            peer_version_message.user_agent,
                            peer_version_message.services,
                            tx.compute_txid()
                        );
                        break;
                    }
                }
                _ => {}
            },
            Ok(Err(read_err)) => {
                error!(
                    "Read error from {} while awaiting Tx confirmation: {}. Possible LIAR detected!",
                    addr, read_err
                );
                break;
            }
            Err(_) => {
                error!(
                    "Timeout waiting for Tx, Inv, or Reject from {} (UA: '{}', Services: {:?}. Possible LIAR detected!",
                    addr, peer_version_message.user_agent, peer_version_message.services
                );
                break;
            }
        }
    }
    Ok(tx_confirmed_by_peer)
}

async fn crawl_seed_node(seed: SocketAddr) -> Result<Vec<SocketAddr>> {
    let mut found_peers = Vec::new();
    info!("crawling seed {:?}", seed);
    let mut stream =
        match tokio::time::timeout(Duration::from_secs(1), TcpStream::connect(seed)).await {
            Ok(Ok(s)) => s,
            _ => return Ok(found_peers),
        };
    stream.set_nodelay(true)?;

    send_msg(
        &mut stream,
        NetworkMessage::Version(build_version_msg(seed)),
    )
    .await?;

    let (mut rd, mut wr) = stream.split();

    info!("waiting for addresses from {:?}...", seed);
    loop {
        let msg = match tokio::time::timeout(Duration::from_secs(3), read_msg(&mut rd)).await {
            Ok(Ok(m)) => m,
            _ => break,
        };
        match msg.payload() {
            NetworkMessage::Version(_) => {
                send_msg(&mut wr, NetworkMessage::Verack).await?;
                send_msg(&mut wr, NetworkMessage::GetAddr).await?;
            }
            NetworkMessage::Addr(list) => {
                let flag = ServiceFlags::from(NODE_LIBRE_RELAY);
                found_peers.extend(list.iter().filter_map(|(_, a)| {
                    if a.services.has(flag) {
                        a.socket_addr().ok()
                    } else {
                        None
                    }
                }));
                break;
            }
            _ => {}
        }
    }
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
