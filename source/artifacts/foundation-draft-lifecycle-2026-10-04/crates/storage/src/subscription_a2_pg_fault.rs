//! Task-owned plaintext loopback PostgreSQL proxy. The server commits a selected
//! real transaction; only its protocol acknowledgement is discarded. No SQL or
//! credentials are recorded. This is a transport fault, not a product hook.
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
pub(super) struct CommitLossProxy {
    pub address: std::net::SocketAddr,
    handle: tokio::task::JoinHandle<()>,
    pub fired: Arc<AtomicBool>,
    pub commit_seen: Arc<tokio::sync::Notify>,
    pub release: Arc<tokio::sync::Notify>,
}
impl Drop for CommitLossProxy {
    fn drop(&mut self) {
        self.handle.abort();
    }
}
async fn packet<R: tokio::io::AsyncRead + Unpin>(r: &mut R) -> std::io::Result<(u8, Vec<u8>)> {
    let kind = r.read_u8().await?;
    let len = r.read_u32().await?;
    if !(4..=16 * 1024 * 1024).contains(&len) {
        return Err(std::io::ErrorKind::InvalidData.into());
    }
    let mut bytes = vec![kind];
    bytes.extend_from_slice(&len.to_be_bytes());
    bytes.resize(len as usize + 1, 0);
    r.read_exact(&mut bytes[5..]).await?;
    Ok((kind, bytes))
}
async fn connection(
    mut client: TcpStream,
    upstream: std::net::SocketAddr,
    fired: Arc<AtomicBool>,
    needle: &'static [u8],
    gate: Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>,
) -> std::io::Result<()> {
    let mut server = TcpStream::connect(upstream).await?;
    // SQLx uses SSLRequest before the ordinary startup packet. Our owned PG has
    // SSL disabled; reject an unexpected TLS negotiation rather than inspect it.
    loop {
        let len = client.read_u32().await?;
        if !(8..=65536).contains(&len) {
            return Err(std::io::ErrorKind::InvalidData.into());
        }
        let mut startup = len.to_be_bytes().to_vec();
        startup.resize(len as usize, 0);
        client.read_exact(&mut startup[4..]).await?;
        let ssl = len == 8 && startup[4..] == 80877103u32.to_be_bytes();
        server.write_all(&startup).await?;
        if ssl {
            let reply = server.read_u8().await?;
            client.write_u8(reply).await?;
            if reply != b'N' {
                return Err(std::io::ErrorKind::Unsupported.into());
            }
            continue;
        }
        break;
    }
    let (mut cr, mut cw) = client.into_split();
    let (mut sr, mut sw) = server.into_split();
    let drop_reply = Arc::new(AtomicBool::new(false));
    let drop_read = drop_reply.clone();
    let forward = async move {
        let mut admitted_insert = false;
        loop {
            let (kind, bytes) = packet(&mut cr).await?;
            if (kind == b'P' || kind == b'Q') && bytes.windows(needle.len()).any(|w| w == needle) {
                admitted_insert = true;
            }
            if admitted_insert
                && kind == b'Q'
                && bytes[5..].starts_with(b"COMMIT\0")
                && !fired.swap(true, Ordering::SeqCst)
            {
                if let Some((seen, release)) = &gate {
                    seen.notify_one();
                    release.notified().await;
                }
                drop_reply.store(true, Ordering::SeqCst);
            }
            sw.write_all(&bytes).await?;
        }
        #[allow(unreachable_code)]
        Ok::<(), std::io::Error>(())
    };
    let backward = async move {
        loop {
            let (kind, bytes) = packet(&mut sr).await?;
            if drop_read.load(Ordering::SeqCst) {
                if kind == b'Z' {
                    cw.shutdown().await?;
                    break;
                }
            } else {
                cw.write_all(&bytes).await?;
            }
        }
        Ok::<(), std::io::Error>(())
    };
    tokio::select! {r=forward=>r,r=backward=>r}
}
impl CommitLossProxy {
    pub async fn start(upstream: std::net::SocketAddr, business: bool) -> Self {
        let needle: &'static [u8] = if business {
            b"INSERT INTO public_subscription_a2_links_v1"
        } else {
            b"INSERT INTO public_subscription_a2_links_v1"
        };
        let commit_seen = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let gate = business.then(|| (commit_seen.clone(), release.clone()));
        assert!(upstream.ip().is_loopback());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let fired = Arc::new(AtomicBool::new(false));
        let flag = fired.clone();
        let handle = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    incoming=listener.accept()=>{let(client,_)=incoming.unwrap();let f=flag.clone();let g=gate.clone();connections.spawn(async move{let _=connection(client,upstream,f,needle,g).await;});},
                    _=connections.join_next(),if !connections.is_empty()=>{}
                }
            }
        });
        Self {
            address,
            handle,
            fired,
            commit_seen,
            release,
        }
    }
}
