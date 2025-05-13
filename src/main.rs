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
use futures::{StreamExt, stream::FuturesUnordered};
use std::{
    collections::HashSet,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpStream, lookup_host},
};
use tracing::info;

const NODE_LIBRE_RELAY: u64 = 1 << 29;

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

    info!("Time to blast some nodes with pigeon poop! 🕊️💩");
    let args = Args::parse();
    let tx_hex_string = args.tx;
    let tx = bitcoin::consensus::deserialize::<Transaction>(&hex::decode(tx_hex_string)?)?;
    let mut tx_raw_msg_bytes = Vec::new();
    RawNetworkMessage::new(Magic::BITCOIN, NetworkMessage::Tx(tx.clone()))
        .consensus_encode(&mut tx_raw_msg_bytes)?;

    let mut seed_addrs = Vec::new();
    let dns_seeds: Vec<&'static str> = vec![
        "dnsseed.bluematt.me",
        "dnsseed.bitcoin.dashjr.org",
        "seed.bitcoinstats.com",
        "seed.btc.petertodd.org",
        "seed.bitcoin.sprovoost.nl",
        "dnsseed.emzy.de",
        "seed.bitcoin.wiz.biz",
    ];

    for seed_host in dns_seeds {
        info!("fetching addrs from {:?}", seed_host);
        if let Ok(resolved_addrs) = lookup_host(format!("{}:8333", seed_host)).await {
            seed_addrs.extend(resolved_addrs);
        }
    }

    info!("found {} potential seed node addresses", seed_addrs.len());

    let mut libre_peers = HashSet::<SocketAddr>::new();
    let mut crawl_tasks = FuturesUnordered::new();
    for addr in seed_addrs {
        crawl_tasks.push(tokio::spawn(crawl_seed_node(addr)));
    }
    while let Some(Ok(Ok(addresses))) = crawl_tasks.next().await {
        libre_peers.extend(addresses);
    }

    let mut poop_delivery_tasks = FuturesUnordered::new();
    for peer_addr in libre_peers.clone() {
        let tx_bytes_clone = tx_raw_msg_bytes.clone();
        poop_delivery_tasks.push(tokio::spawn(async move {
            let _ = deliver_poop_tx(peer_addr, &tx_bytes_clone).await;
            peer_addr
        }));
    }

    info!("pooping on {} libre nodes", libre_peers.len());
    while let Some(Ok(addr)) = poop_delivery_tasks.next().await {
        info!("you pooped on libre node {}", addr);
    }

    info!(
        "TX: {:?} blasted to {} libre relay nodes. GLHF",
        tx.compute_txid(),
        libre_peers.len(),
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
        tokio::time::timeout(Duration::from_millis(500), TcpStream::connect(addr)).await??;
    stream.set_nodelay(true)?;

    send_msg(
        &mut stream,
        NetworkMessage::Version(build_version_msg(addr)),
    )
    .await?;

    let (mut rd, mut wr) = stream.split();
    tokio::time::timeout(Duration::from_millis(1000), async {
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
        match tokio::time::timeout(Duration::from_millis(500), TcpStream::connect(seed)).await {
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
        let msg = match tokio::time::timeout(Duration::from_millis(1000), read_msg(&mut rd)).await {
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
