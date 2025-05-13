use anyhow::Result;
use bitcoin::{
    Transaction,
    consensus::{Decodable, Encodable},
    io::Cursor,
    p2p::{
        Address, Magic, ServiceFlags,
        message::{NetworkMessage, RawNetworkMessage},
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
        let tx_bytes_clone = tx_raw_msg_bytes.clone();
        let permit = semaphore.clone().acquire_owned().await?;
        poop_delivery_tasks.spawn(async move {
            let result = match deliver_poop_tx(peer_addr, &tx_bytes_clone).await {
                Ok(_) => Ok(peer_addr),
                Err(e) => Err((peer_addr, e.to_string())),
            };
            drop(permit);
            result
        });
    }

    info!("pooping on {} libre nodes", libre_peers.len());
    let mut success_count = 0;

    while let Some(res) = poop_delivery_tasks.join_next().await {
        match res {
            Ok(Ok(addr)) => {
                info!("you pooped on libre node {}", addr);
                success_count += 1;
            }
            Ok(Err((addr, deliver_error))) => {
                error!("failed to deliver poop to {}: {}", addr, deliver_error);
            }
            Err(join_error) => {
                error!("join error during poop delivery: {}", join_error);
                if join_error.is_panic() {
                    error!("a poop delivery task panicked!");
                }
            }
        }
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
        user_agent: "/Satoshi:29.0.0/".into(),
        start_height: 1337,
        relay: true,
    }
}

async fn deliver_poop_tx(addr: SocketAddr, tx_bytes: &[u8]) -> Result<()> {
    let mut stream =
        tokio::time::timeout(Duration::from_secs(1), TcpStream::connect(addr)).await??;
    stream.set_nodelay(true)?;

    send_msg(
        &mut stream,
        NetworkMessage::Version(build_version_msg(addr)),
    )
    .await?;

    let (mut rd, mut wr) = stream.split();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let NetworkMessage::Version(_) = read_msg(&mut rd).await?.payload() {
                break Ok::<(), anyhow::Error>(());
            }
        }
    })
    .await??;

    send_msg(&mut wr, NetworkMessage::Verack).await?;
    wr.write_all(tx_bytes).await?;
    wr.flush().await?;
    Ok(())
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
        let msg = match tokio::time::timeout(Duration::from_secs(2), read_msg(&mut rd)).await {
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
