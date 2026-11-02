//! Owned loopback PostgreSQL protocol fault; never logs SQL/credentials.
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
pub(super) struct Proxy {
    pub address: std::net::SocketAddr,
    pub fired: Arc<AtomicBool>,
    handle: tokio::task::JoinHandle<()>,
}
impl Drop for Proxy {
    fn drop(&mut self) {
        self.handle.abort();
    }
}
async fn packet<R: tokio::io::AsyncRead + Unpin>(r: &mut R) -> std::io::Result<(u8, Vec<u8>)> {
    let tag = r.read_u8().await?;
    let n = r.read_u32().await?;
    if !(4..=16 * 1024 * 1024).contains(&n) {
        return Err(std::io::ErrorKind::InvalidData.into());
    }
    let mut bytes = vec![tag];
    bytes.extend_from_slice(&n.to_be_bytes());
    bytes.resize(n as usize + 1, 0);
    r.read_exact(&mut bytes[5..]).await?;
    Ok((tag, bytes))
}
async fn connection(
    mut client: TcpStream,
    upstream: std::net::SocketAddr,
    which: usize,
    forward_commit: bool,
    count: Arc<AtomicUsize>,
    fired: Arc<AtomicBool>,
) -> std::io::Result<()> {
    let mut server = TcpStream::connect(upstream).await?;
    loop {
        let n = client.read_u32().await?;
        if !(8..=65536).contains(&n) {
            return Err(std::io::ErrorKind::InvalidData.into());
        }
        let mut bytes = n.to_be_bytes().to_vec();
        bytes.resize(n as usize, 0);
        client.read_exact(&mut bytes[4..]).await?;
        let ssl = n == 8 && bytes[4..] == 80877103u32.to_be_bytes();
        server.write_all(&bytes).await?;
        if ssl {
            let response = server.read_u8().await?;
            client.write_u8(response).await?;
            if response != b'N' {
                return Err(std::io::ErrorKind::Unsupported.into());
            }
            continue;
        }
        break;
    }
    let (mut cr, mut cw) = client.into_split();
    let (mut sr, mut sw) = server.into_split();
    let discard = Arc::new(AtomicBool::new(false));
    let read_discard = discard.clone();
    let forward = async move {
        let mut member_write = false;
        loop {
            let (tag, bytes) = packet(&mut cr).await?;
            if matches!(tag, b'P' | b'Q')
                && bytes
                    .windows(b".subscription_runtime_records SET".len())
                    .any(|s| s == b".subscription_runtime_records SET")
            {
                member_write = true;
            }
            if member_write && tag == b'Q' && bytes[5..].starts_with(b"COMMIT\0") {
                let nth = count.fetch_add(1, Ordering::SeqCst) + 1;
                if nth == which && !fired.swap(true, Ordering::SeqCst) {
                    if !forward_commit {
                        sw.shutdown().await?;
                        return Ok::<(), std::io::Error>(());
                    }
                    discard.store(true, Ordering::SeqCst);
                }
            }
            sw.write_all(&bytes).await?;
        }
    };
    let backward = async move {
        loop {
            let (tag, bytes) = packet(&mut sr).await?;
            if read_discard.load(Ordering::SeqCst) {
                if tag == b'Z' {
                    cw.shutdown().await?;
                    return Ok::<(), std::io::Error>(());
                }
            } else {
                cw.write_all(&bytes).await?;
            }
        }
    };
    tokio::select! {r=forward=>r,r=backward=>r}
}
impl Proxy {
    pub async fn start(upstream: std::net::SocketAddr, which: usize, forward_commit: bool) -> Self {
        assert!(upstream.ip().is_loopback());
        assert!(which == 1 || which == 2);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let fired = Arc::new(AtomicBool::new(false));
        let f = fired.clone();
        let count = Arc::new(AtomicUsize::new(0));
        let handle = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    r=listener.accept()=>{let(c,_)=r.unwrap();let f=f.clone();let count=count.clone();connections.spawn(async move{let _=connection(c,upstream,which,forward_commit,count,f).await;});},
                    _=connections.join_next(),if !connections.is_empty()=>{}
                }
            }
        });
        Self {
            address,
            fired,
            handle,
        }
    }
}
