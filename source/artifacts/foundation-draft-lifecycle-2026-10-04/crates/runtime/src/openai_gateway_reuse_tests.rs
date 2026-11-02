use super::*;
use axum::extract::ConnectInfo;
use std::{collections::HashSet, net::SocketAddr, sync::atomic::{AtomicBool, AtomicUsize, Ordering}, time::Instant};

#[derive(Default)]
struct Connections {
    peers: Mutex<HashSet<SocketAddr>>,
    calls: AtomicUsize,
    slow: AtomicBool,
    requests: Mutex<Vec<Value>>,
}
async fn reply(
    State(state): State<Arc<Connections>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(request): Json<Value>,
) -> Json<Value> {
    state.calls.fetch_add(1, Ordering::SeqCst);
    let first = state.peers.lock().unwrap().insert(peer);
    state.requests.lock().unwrap().push(request);
    if state.slow.load(Ordering::SeqCst) { tokio::time::sleep(Duration::from_secs(2)).await; }
    else if first { tokio::time::sleep(Duration::from_millis(120)).await; }
    Json(envelope(plan().to_string()))
}
async fn endpoint() -> (Arc<Connections>, String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1/chat/completions",listener.local_addr().unwrap());
    let state = Arc::new(Connections::default());
    let app = Router::new().route("/v1/chat/completions",post(reply)).with_state(state.clone());
    let task = tokio::spawn(async move { axum::serve(listener,app.into_make_service_with_connect_info::<SocketAddr>()).await.unwrap(); });
    (state,url,task)
}
fn config(url:&str) -> OpenAiGatewayConfig {
    OpenAiGatewayConfig::new(url,"transport-fixture",Zeroizing::new("fixture-key-not-a-provider-key".into()),Duration::from_secs(1)).unwrap()
}
#[tokio::test]
async fn sequential_requests_reuse_tcp_and_schema_is_transmitted_once() {
    let (state,url,task) = endpoint().await;
    let gateway = OpenAiGateway::new(config(&url)).unwrap();
    let mut elapsed=vec![];
    for _ in 0..2 {
        let now=Instant::now();
        gateway.generate_with_context(&input(),&context()).await.unwrap();
        elapsed.push(now.elapsed().as_millis());
    }
    assert_eq!(state.calls.load(Ordering::SeqCst),2);
    assert_eq!(state.peers.lock().unwrap().len(),1);
    let requests=state.requests.lock().unwrap();
    let system=requests[0]["messages"][0]["content"].as_str().unwrap();
    let protocol:Value=serde_json::from_str(system.split("Protocol data: ").nth(1).unwrap()).unwrap();
    let mut duplicate=protocol.clone();
    duplicate["approved_tool_definitions"]=serde_json::to_value(input().tool_definitions).unwrap();
    let compact_bytes=serde_json::to_vec(&protocol).unwrap().len();
    let duplicate_bytes=serde_json::to_vec(&duplicate).unwrap().len();
    assert!(compact_bytes<duplicate_bytes);
    println!("REUSE_FIXTURE {}",json!({"requests":2,"tcp_connections":1,"elapsed_ms":elapsed,"protocol_bytes":compact_bytes,"same_protocol_with_duplicate_schema_bytes":duplicate_bytes,"injected_first_connection_delay_ms":120,"live_supplier":false}));
    drop(requests);
    // Same HTTP/1.1 fixture, same protocol, only the previous no-idle setting:
    // comparison measures TCP reuse, never pretends to measure supplier latency.
    let baseline=OpenAiGateway { client:Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none()).http1_only().retry(reqwest::retry::never()).pool_max_idle_per_host(0).connect_timeout(Duration::from_secs(1)).timeout(Duration::from_secs(1)).build().unwrap(), config:config(&url) };
    for _ in 0..2 { baseline.generate_with_context(&input(),&context()).await.unwrap(); }
    assert_eq!(state.calls.load(Ordering::SeqCst),4);
    assert_eq!(state.peers.lock().unwrap().len(),3);
    task.abort();let _=task.await;
}
#[tokio::test]
async fn cancelled_and_timed_out_requests_do_not_retry_or_poison_later_queries() {
    let (state,url,task)=endpoint().await;
    state.slow.store(true,Ordering::SeqCst);
    let gateway=Arc::new(OpenAiGateway::new(config(&url)).unwrap());
    let g=gateway.clone();
    let pending=tokio::spawn(async move { g.generate_with_context(&input(),&context()).await });
    tokio::time::timeout(Duration::from_secs(1),async {while state.calls.load(Ordering::SeqCst)<1 {tokio::task::yield_now().await;}}).await.unwrap();
    pending.abort();assert!(pending.await.unwrap_err().is_cancelled());
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(state.calls.load(Ordering::SeqCst),1);
    assert!(gateway.generate_with_context(&input(),&context()).await.is_err());
    assert_eq!(state.calls.load(Ordering::SeqCst),2);
    state.slow.store(false,Ordering::SeqCst);
    gateway.generate_with_context(&input(),&context()).await.unwrap();
    assert_eq!(state.calls.load(Ordering::SeqCst),3);
    println!("CANCEL_FIXTURE {}",json!({"cancelled":1,"timed_out":1,"later_success":1,"requests":3,"automatic_retries":0}));
    task.abort();let _=task.await;
}
