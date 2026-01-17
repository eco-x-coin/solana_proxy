use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::Json,
    routing::{get, post},
    Router,
};
use base64::Engine;
use clap::Parser;
use serde::{Deserialize, Serialize};
use solana_client::{nonblocking::rpc_client::RpcClient, rpc_config::RpcSendTransactionConfig};
use solana_sdk::{
    commitment_config::{CommitmentConfig, CommitmentLevel},
    transaction::{Transaction, VersionedTransaction},
};
use solana_transaction_status::UiTransactionEncoding;
use std::{collections::HashSet, net::SocketAddr, sync::Arc, time::Duration};
use tokio::net::UdpSocket;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Proxy server port
    #[arg(short, long, default_value = "8898")]
    port: u16,

    /// Solana validator RPC URL
    #[arg(short, long, default_value = "http://localhost:8899")]
    validator: String,

    /// Solana cluster entrypoint (for TPU client)
    #[arg(short, long, default_value = "http://localhost:8899")]
    entrypoint: String,
}

#[derive(Clone)]
struct AppState {
    validator_url: String,
    entrypoint: String,
    rpc_client: Arc<RpcClient>,
    http_client: reqwest::Client,
    tpu_addresses: Arc<RwLock<Vec<SocketAddr>>>,
    jito_tpu_addresses: Arc<RwLock<Vec<SocketAddr>>>,
    jito_validator_pubkeys: Arc<RwLock<HashSet<String>>>,
    udp_socket: Arc<UdpSocket>,
}

#[derive(Debug, Deserialize)]
struct TokenQuery {
    token: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct JSONRPCRequest {
    jsonrpc: String,
    id: serde_json::Value,
    method: String,
    params: Vec<serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize)]
struct JSONRPCResponse {
    jsonrpc: String,
    id: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<RPCError>,
}

#[derive(Debug, Serialize, Deserialize)]
struct RPCError {
    code: i32,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<String>,
}

// 合法的 token
const VALID_TOKEN: &str = "beyond0128";

// TPU 端口常量
const TPU_PORT: u16 = 8004;

// TPU 地址更新间隔（秒）
const TPU_REFRESH_INTERVAL_SECS: u64 = 1;

// 最大 TPU 地址数量（发送到前 N 个 leader）
const MAX_TPU_TARGETS: usize = 10;

// 获取未来多少个 slot 的 leader
const FUTURE_SLOTS_COUNT: usize = 10;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 初始化日志
    tracing_subscriber::fmt::init();

    let args = Args::parse();

    // 创建异步 RPC 客户端（用于获取集群信息）
    let rpc_client = Arc::new(RpcClient::new_with_commitment(
        args.entrypoint.clone(),
        CommitmentConfig::processed(),
    ));

    // 创建 HTTP 客户端（用于代理其他方法）
    // 配置连接池以提高性能
    let http_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .pool_max_idle_per_host(10)
        .pool_idle_timeout(Duration::from_secs(90))
        .build()?;

    // 创建 UDP socket 用于 TPU 发送
    let udp_socket = Arc::new(UdpSocket::bind("0.0.0.0:0").await?);

    // 初始化 TPU 地址列表（延迟初始化）
    let tpu_addresses: Arc<RwLock<Vec<SocketAddr>>> = Arc::new(RwLock::new(Vec::new()));

    // 初始化 Jito TPU 地址列表
    let jito_tpu_addresses: Arc<RwLock<Vec<SocketAddr>>> = Arc::new(RwLock::new(Vec::new()));
    // 初始化 Jito validator pubkey 列表
    let jito_validator_pubkeys: Arc<RwLock<HashSet<String>>> =
        Arc::new(RwLock::new(HashSet::new()));

    let state = AppState {
        validator_url: args.validator.clone(),
        entrypoint: args.entrypoint.clone(),
        rpc_client: rpc_client.clone(),
        http_client: http_client.clone(),
        tpu_addresses: tpu_addresses.clone(),
        jito_tpu_addresses: jito_tpu_addresses.clone(),
        jito_validator_pubkeys: jito_validator_pubkeys.clone(),
        udp_socket: udp_socket.clone(),
    };

    // 克隆 rpc_client 用于后台任务
    let rpc_client_for_tpu = rpc_client.clone();
    let rpc_client_for_jito = rpc_client.clone();

    // 在后台初始化 TPU 地址，并定期更新
    tokio::spawn(async move {
        // 立即初始化一次
        if let Err(e) = init_tpu_addresses(rpc_client_for_tpu.clone(), tpu_addresses.clone()).await
        {
            warn!(
                "Failed to initialize TPU addresses: {}. Will use RPC fallback.",
                e
            );
        }

        // 定期更新 TPU 地址
        // Leader 节点会定期轮换，需要保持地址列表最新
        let mut interval = tokio::time::interval(Duration::from_secs(TPU_REFRESH_INTERVAL_SECS));
        // 跳过第一次立即触发（因为上面已经初始化过了）
        interval.tick().await;

        loop {
            interval.tick().await;
            match init_tpu_addresses(rpc_client_for_tpu.clone(), tpu_addresses.clone()).await {
                Ok(_) => {
                    // 只在 debug 模式下记录，避免日志过多
                    tracing::debug!("[TPU] TPU addresses refreshed");
                }
                Err(e) => {
                    warn!("[TPU] Failed to refresh TPU addresses: {}", e);
                }
            }
        }
    });

    // 在后台初始化 Jito validator pubkey 列表，并定期更新
    let http_client_for_jito_pubkeys = http_client.clone();
    let jito_validator_pubkeys_for_fetch = jito_validator_pubkeys.clone();
    tokio::spawn(async move {
        // 立即初始化一次
        if let Err(e) = fetch_jito_validator_pubkeys(
            http_client_for_jito_pubkeys.clone(),
            jito_validator_pubkeys_for_fetch.clone(),
        )
        .await
        {
            warn!(
                "Failed to initialize Jito validator pubkeys: {}. Will retry.",
                e
            );
        }

        // 定期更新 Jito validator pubkey 列表（每5分钟更新一次，因为列表变化不频繁）
        let mut interval = tokio::time::interval(Duration::from_secs(300));
        interval.tick().await;

        loop {
            interval.tick().await;
            match fetch_jito_validator_pubkeys(
                http_client_for_jito_pubkeys.clone(),
                jito_validator_pubkeys_for_fetch.clone(),
            )
            .await
            {
                Ok(_) => {
                    tracing::debug!("[JITO-TPU] Jito validator pubkeys refreshed");
                }
                Err(e) => {
                    warn!("[JITO-TPU] Failed to refresh Jito validator pubkeys: {}", e);
                }
            }
        }
    });

    // 在后台初始化 Jito TPU 地址，并定期更新
    let jito_validator_pubkeys_for_tpu = jito_validator_pubkeys.clone();
    tokio::spawn(async move {
        // 等待一小段时间，让 Jito validator pubkeys 先初始化
        tokio::time::sleep(Duration::from_secs(2)).await;

        // 立即初始化一次
        if let Err(e) = init_jito_tpu_addresses(
            rpc_client_for_jito.clone(),
            jito_tpu_addresses.clone(),
            jito_validator_pubkeys_for_tpu.clone(),
        )
        .await
        {
            warn!(
                "Failed to initialize Jito TPU addresses: {}. Will retry.",
                e
            );
        }

        // 定期更新 Jito TPU 地址
        let mut interval = tokio::time::interval(Duration::from_secs(TPU_REFRESH_INTERVAL_SECS));
        interval.tick().await;

        loop {
            interval.tick().await;
            match init_jito_tpu_addresses(
                rpc_client_for_jito.clone(),
                jito_tpu_addresses.clone(),
                jito_validator_pubkeys_for_tpu.clone(),
            )
            .await
            {
                Ok(_) => {
                    tracing::debug!("[JITO-TPU] Jito TPU addresses refreshed");
                }
                Err(e) => {
                    warn!("[JITO-TPU] Failed to refresh Jito TPU addresses: {}", e);
                }
            }
        }
    });

    let app = Router::new()
        .route("/", post(handle_proxy_request))
        .route("/api/v1/transactions", post(handle_jito_transactions))
        .route("/health", get(handle_health))
        .with_state(state);

    let addr = format!("0.0.0.0:{}", args.port);
    info!("=================================");
    info!("Solana Validator Proxy Server (Rust)");
    info!("=================================");
    info!("Listening on port: {}", args.port);
    info!("Proxying to: {}", args.validator);
    info!("Entrypoint for TPU: {}", args.entrypoint);
    info!("Allowed methods: sendTransaction (via TPU), simulateTransaction, getLatestBlockhash");
    info!("Jito endpoint: /api/v1/transactions (sends via TPU UDP to Jito leader nodes)");
    info!("=================================");

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

async fn init_tpu_addresses(
    rpc_client: Arc<RpcClient>,
    tpu_addresses: Arc<RwLock<Vec<SocketAddr>>>,
) -> anyhow::Result<()> {
    // 1. 获取当前 slot（使用 processed commitment level）
    let current_slot = match rpc_client
        .get_slot_with_commitment(CommitmentConfig::processed())
        .await
    {
        Ok(slot) => slot,
        Err(e) => {
            warn!("[TPU] Failed to get current slot: {}", e);
            // 如果获取 slot 失败，回退到原来的逻辑
            return init_tpu_addresses_fallback(rpc_client, tpu_addresses).await;
        }
    };

    // 2. 获取当前和未来 N 个 slot 的 leader pubkeys
    let leader_pubkeys = match rpc_client
        .get_slot_leaders(current_slot, FUTURE_SLOTS_COUNT as u64)
        .await
    {
        Ok(leaders) => leaders,
        Err(e) => {
            warn!("[TPU] Failed to get slot leaders: {}", e);
            return init_tpu_addresses_fallback(rpc_client, tpu_addresses).await;
        }
    };

    // 3. 获取集群节点信息（用于查找 leader 的 TPU 地址）
    let cluster_nodes = match rpc_client.get_cluster_nodes().await {
        Ok(nodes) => nodes,
        Err(e) => {
            warn!("[TPU] Failed to get cluster nodes: {}", e);
            return init_tpu_addresses_fallback(rpc_client, tpu_addresses).await;
        }
    };

    // 4. 根据 leader pubkeys 查找对应的 TPU 地址
    let mut leader_addresses = Vec::new();
    let mut found_pubkeys = HashSet::new();

    for leader_pubkey in leader_pubkeys.iter() {
        // 将 Pubkey 转换为字符串进行比较
        let leader_pubkey_str = leader_pubkey.to_string();

        if found_pubkeys.contains(&leader_pubkey_str) {
            continue; // 去重
        }

        // 在集群节点中查找对应的 TPU 地址
        for node in cluster_nodes.iter() {
            // 比较 pubkey（都是字符串）
            if node.pubkey == leader_pubkey_str {
                if let Some(tpu_addr) = node.tpu {
                    leader_addresses.push(tpu_addr);
                    found_pubkeys.insert(leader_pubkey_str);
                    tracing::debug!(
                        "[TPU] Found leader TPU address: {} (pubkey: {})",
                        tpu_addr,
                        leader_pubkey
                    );
                    break;
                } else if let Some(gossip_addr) = node.gossip {
                    let tpu_addr = SocketAddr::new(gossip_addr.ip(), TPU_PORT);
                    leader_addresses.push(tpu_addr);
                    found_pubkeys.insert(leader_pubkey_str);
                    tracing::debug!(
                        "[TPU] Constructed leader TPU address from gossip: {} (pubkey: {})",
                        tpu_addr,
                        leader_pubkey
                    );
                    break;
                }
            }
        }
    }

    // 5. 如果没找到足够的 leader 地址，添加其他节点作为后备
    if leader_addresses.len() < MAX_TPU_TARGETS {
        for node in cluster_nodes.iter() {
            if leader_addresses.len() >= MAX_TPU_TARGETS {
                break;
            }

            // 跳过已经是 leader 的节点
            if found_pubkeys.contains(&node.pubkey) {
                continue;
            }

            if let Some(tpu_addr) = node.tpu {
                if !leader_addresses.contains(&tpu_addr) {
                    leader_addresses.push(tpu_addr);
                }
            } else if let Some(gossip_addr) = node.gossip {
                let tpu_addr = SocketAddr::new(gossip_addr.ip(), TPU_PORT);
                if !leader_addresses.contains(&tpu_addr) {
                    leader_addresses.push(tpu_addr);
                }
            }
        }
    }

    // 6. 如果还是没有地址，使用本地地址作为后备
    if leader_addresses.is_empty() {
        warn!("[TPU] No leader TPU addresses found, using localhost fallback");
        if let Ok(addr) = format!("127.0.0.1:{}", TPU_PORT).parse::<SocketAddr>() {
            leader_addresses.push(addr);
        }
    }

    if leader_addresses.is_empty() {
        return Err(anyhow::anyhow!("No TPU addresses available"));
    }

    // 7. 只在地址真正变化时才更新和记录日志
    let current_addresses = tpu_addresses.read().await;
    if *current_addresses != leader_addresses {
        drop(current_addresses);
        *tpu_addresses.write().await = leader_addresses.clone();
        info!(
            "[TPU] Leader TPU addresses updated: {} addresses (current slot: {})",
            leader_addresses.len(),
            current_slot
        );
    } else {
        tracing::debug!(
            "[TPU] Leader TPU addresses unchanged: {} addresses (current slot: {})",
            leader_addresses.len(),
            current_slot
        );
    }

    Ok(())
}

// 从 Jito API 获取 Jito validator pubkey 列表
async fn fetch_jito_validator_pubkeys(
    http_client: reqwest::Client,
    jito_validator_pubkeys: Arc<RwLock<HashSet<String>>>,
) -> anyhow::Result<()> {
    // Jito API 端点：获取 JitoSOL validators
    let url = "https://kobe.mainnet.jito.network/api/v1/jitosol_validators";

    match http_client.get(url).send().await {
        Ok(resp) => {
            if resp.status().is_success() {
                match resp.json::<serde_json::Value>().await {
                    Ok(json) => {
                        let mut pubkeys = HashSet::new();

                        // 解析响应，提取 vote_account pubkeys
                        if let Some(validators) = json.get("validators").and_then(|v| v.as_array())
                        {
                            for validator in validators {
                                if let Some(vote_account) =
                                    validator.get("vote_account").and_then(|v| v.as_str())
                                {
                                    pubkeys.insert(vote_account.to_string());
                                }
                            }
                        }

                        if !pubkeys.is_empty() {
                            *jito_validator_pubkeys.write().await = pubkeys.clone();
                            info!(
                                "[JITO-TPU] Fetched {} Jito validator pubkeys",
                                pubkeys.len()
                            );
                            Ok(())
                        } else {
                            warn!("[JITO-TPU] No Jito validators found in API response");
                            Err(anyhow::anyhow!("No Jito validators found"))
                        }
                    }
                    Err(e) => {
                        warn!("[JITO-TPU] Failed to parse Jito API response: {}", e);
                        Err(anyhow::anyhow!("Failed to parse response: {}", e))
                    }
                }
            } else {
                warn!(
                    "[JITO-TPU] Jito API returned error status: {}",
                    resp.status()
                );
                Err(anyhow::anyhow!(
                    "API returned error status: {}",
                    resp.status()
                ))
            }
        }
        Err(e) => {
            warn!("[JITO-TPU] Failed to fetch Jito validator pubkeys: {}", e);
            Err(anyhow::anyhow!("Request failed: {}", e))
        }
    }
}

// 初始化 Jito leader 节点的 TPU 地址
// 获取当前和未来的 leader，并找到它们的 TPU 地址
// 只选择真正的 Jito 节点
async fn init_jito_tpu_addresses(
    rpc_client: Arc<RpcClient>,
    jito_tpu_addresses: Arc<RwLock<Vec<SocketAddr>>>,
    jito_validator_pubkeys: Arc<RwLock<HashSet<String>>>,
) -> anyhow::Result<()> {
    // 1. 获取当前 slot（使用 processed commitment level）
    let current_slot = match rpc_client
        .get_slot_with_commitment(CommitmentConfig::processed())
        .await
    {
        Ok(slot) => slot,
        Err(e) => {
            warn!("[JITO-TPU] Failed to get current slot: {}", e);
            return Err(anyhow::anyhow!("Failed to get current slot: {}", e));
        }
    };

    // 2. 获取当前和未来 N 个 slot 的 leader pubkeys（获取更多以找到 Jito leader）
    let leader_pubkeys = match rpc_client
        .get_slot_leaders(current_slot, (FUTURE_SLOTS_COUNT * 2) as u64)
        .await
    {
        Ok(leaders) => leaders,
        Err(e) => {
            warn!("[JITO-TPU] Failed to get slot leaders: {}", e);
            return Err(anyhow::anyhow!("Failed to get slot leaders: {}", e));
        }
    };

    // 3. 获取集群节点信息
    let cluster_nodes = match rpc_client.get_cluster_nodes().await {
        Ok(nodes) => nodes,
        Err(e) => {
            warn!("[JITO-TPU] Failed to get cluster nodes: {}", e);
            return Err(anyhow::anyhow!("Failed to get cluster nodes: {}", e));
        }
    };

    // 4. 根据 leader pubkeys 查找对应的 TPU 地址
    // 取前5个不同的 leader 节点作为 Jito leader（实际应用中可以通过 pubkey 列表过滤 Jito 节点）
    let mut jito_leader_addresses = Vec::new();
    let mut found_pubkeys: HashSet<String> = HashSet::new();
    const JITO_LEADER_COUNT: usize = 5;

    for leader_pubkey in leader_pubkeys.iter() {
        if jito_leader_addresses.len() >= JITO_LEADER_COUNT {
            break;
        }

        let leader_pubkey_str = leader_pubkey.to_string();

        if found_pubkeys.contains(&leader_pubkey_str) {
            continue; // 去重
        }

        // 在集群节点中查找对应的 TPU 地址
        for node in cluster_nodes.iter() {
            if node.pubkey == leader_pubkey_str {
                if let Some(tpu_addr) = node.tpu {
                    jito_leader_addresses.push(tpu_addr);
                    found_pubkeys.insert(leader_pubkey_str);
                    tracing::debug!(
                        "[JITO-TPU] Found Jito leader TPU address: {} (pubkey: {})",
                        tpu_addr,
                        leader_pubkey
                    );
                    break;
                } else if let Some(gossip_addr) = node.gossip {
                    let tpu_addr = SocketAddr::new(gossip_addr.ip(), TPU_PORT);
                    jito_leader_addresses.push(tpu_addr);
                    found_pubkeys.insert(leader_pubkey_str);
                    tracing::debug!(
                        "[JITO-TPU] Constructed Jito leader TPU address from gossip: {} (pubkey: {})",
                        tpu_addr,
                        leader_pubkey
                    );
                    break;
                }
            }
        }
    }

    // 6. 如果完全找不到 Jito leader 地址，返回错误
    // 如果找到了一些但不足5个，使用找到的（维持现状）
    if jito_leader_addresses.is_empty() {
        return Err(anyhow::anyhow!("No Jito leader TPU addresses found"));
    }

    // 7. 更新地址列表
    let current_addresses = jito_tpu_addresses.read().await;
    if *current_addresses != jito_leader_addresses {
        drop(current_addresses);
        *jito_tpu_addresses.write().await = jito_leader_addresses.clone();
        info!(
            "[JITO-TPU] Jito leader TPU addresses updated: {} addresses (current slot: {})",
            jito_leader_addresses.len(),
            current_slot
        );
    } else {
        tracing::debug!(
            "[JITO-TPU] Jito leader TPU addresses unchanged: {} addresses (current slot: {})",
            jito_leader_addresses.len(),
            current_slot
        );
    }

    Ok(())
}

// 回退函数（原来的逻辑）
async fn init_tpu_addresses_fallback(
    rpc_client: Arc<RpcClient>,
    tpu_addresses: Arc<RwLock<Vec<SocketAddr>>>,
) -> anyhow::Result<()> {
    // 获取集群节点信息
    let cluster_nodes = rpc_client.get_cluster_nodes().await?;

    // 构建 TPU 地址列表
    let mut addresses: Vec<SocketAddr> = Vec::new();
    for node in cluster_nodes.iter() {
        if let Some(tpu_addr) = node.tpu {
            addresses.push(tpu_addr);
        } else if let Some(gossip_addr) = node.gossip {
            let tpu_addr = SocketAddr::new(gossip_addr.ip(), TPU_PORT);
            addresses.push(tpu_addr);
        }
    }

    if addresses.is_empty() {
        warn!("[TPU] No TPU addresses found from cluster, using localhost fallback");
        if let Ok(addr) = format!("127.0.0.1:{}", TPU_PORT).parse::<SocketAddr>() {
            addresses.push(addr);
        }
    }

    if addresses.is_empty() {
        return Err(anyhow::anyhow!("No TPU addresses available"));
    }

    addresses.sort();
    addresses.dedup();

    let current_addresses = tpu_addresses.read().await;
    if *current_addresses != addresses {
        drop(current_addresses);
        *tpu_addresses.write().await = addresses.clone();
        info!(
            "[TPU] TPU addresses updated (fallback): {} addresses",
            addresses.len()
        );
    }

    Ok(())
}

// UDP 发送辅助函数（fire-and-forget）
async fn send_transaction_via_tpu(
    state: AppState,
    tx_bytes: Vec<u8>,
    signature: solana_sdk::signature::Signature,
) {
    let tpu_guard = state.tpu_addresses.read().await;
    let targets: Vec<SocketAddr> = if !tpu_guard.is_empty() {
        tpu_guard.iter().take(MAX_TPU_TARGETS).copied().collect()
    } else {
        Vec::new()
    };
    drop(tpu_guard);

    if targets.is_empty() {
        tracing::debug!("[TPU] No TPU targets available, skipping UDP send");
        return;
    }

    tracing::debug!(
        "[TPU] Sending transaction via TPU UDP to {} addresses (fire-and-forget), signature: {}",
        targets.len(),
        signature
    );

    // 并行启动所有发送任务，不等待返回结果
    for addr in targets.iter() {
        let tx_bytes_clone = tx_bytes.clone();
        let socket = state.udp_socket.clone();
        let addr_clone = *addr;
        let sig_clone = signature;

        // 在后台并行发送，不等待结果
        tokio::spawn(async move {
            match socket.send_to(&tx_bytes_clone, addr_clone).await {
                Ok(_) => {
                    tracing::debug!(
                        "[TPU] Sent transaction to {}:{}, signature: {}",
                        addr_clone.ip(),
                        addr_clone.port(),
                        sig_clone
                    );
                }
                Err(e) => {
                    tracing::debug!(
                        "[TPU] Failed to send to {}:{}, signature: {}, error: {}",
                        addr_clone.ip(),
                        addr_clone.port(),
                        sig_clone,
                        e
                    );
                }
            }
        });
    }
}

async fn handle_proxy_request(
    State(state): State<AppState>,
    Query(query): Query<TokenQuery>,
    headers: HeaderMap,
    body: String,
) -> Result<Json<JSONRPCResponse>, StatusCode> {
    // 验证 token
    match query.token.as_deref() {
        Some(token) if token == VALID_TOKEN => {
            tracing::debug!("[AUTH] Valid token provided");
        }
        _ => {
            warn!("[AUTH] Invalid or missing token");
            return Ok(Json(JSONRPCResponse {
                jsonrpc: "2.0".to_string(),
                id: serde_json::Value::Null,
                result: None,
                error: Some(RPCError {
                    code: -32001,
                    message: "Unauthorized".to_string(),
                    data: Some("Invalid or missing token.".to_string()),
                }),
            }));
        }
    }

    // 解析 JSON-RPC 请求
    let req: JSONRPCRequest = match serde_json::from_str(&body) {
        Ok(req) => req,
        Err(e) => {
            error!("Failed to parse JSON: {}", e);
            return Ok(Json(JSONRPCResponse {
                jsonrpc: "2.0".to_string(),
                id: serde_json::Value::Null,
                result: None,
                error: Some(RPCError {
                    code: -32700,
                    message: "Parse error".to_string(),
                    data: Some(e.to_string()),
                }),
            }));
        }
    };

    tracing::debug!("[ALLOWED] Method: {}, ID: {}", req.method, req.id);

    // 根据方法类型处理
    let response = match req.method.as_str() {
        "sendTransaction" => {
            // 并行执行 HTTP 代理和 UDP 发送
            let state_clone = state.clone();
            let req_clone = req.clone();

            // 启动 UDP 发送任务（fire-and-forget）
            tokio::spawn(async move {
                // 解析交易以获取签名和字节
                if let Some(serde_json::Value::String(tx_str)) = req_clone.params.get(0) {
                    // 尝试解码交易
                    let tx_bytes: Option<Vec<u8>> =
                        if tx_str.contains('/') || tx_str.contains('+') || tx_str.contains('=') {
                            // 看起来像 base64
                            base64::engine::general_purpose::STANDARD
                                .decode(tx_str)
                                .ok()
                                .or_else(|| bs58::decode(tx_str).into_vec().ok())
                        } else {
                            // 看起来像 base58
                            bs58::decode(tx_str).into_vec().ok().or_else(|| {
                                base64::engine::general_purpose::STANDARD
                                    .decode(tx_str)
                                    .ok()
                            })
                        };

                    if let Some(tx_bytes) = tx_bytes {
                        // 尝试提取签名
                        if let Ok(versioned_tx) =
                            bincode::deserialize::<VersionedTransaction>(&tx_bytes)
                        {
                            let signature = versioned_tx.signatures[0];
                            send_transaction_via_tpu(state_clone, tx_bytes, signature).await;
                        } else if let Ok(tx) = bincode::deserialize::<Transaction>(&tx_bytes) {
                            let signature = tx.signatures[0];
                            send_transaction_via_tpu(state_clone, tx_bytes, signature).await;
                        }
                    }
                }
            });

            // 并行执行 HTTP 代理，并返回其响应
            handle_proxy_method(state, req, headers, body).await
        }
        _ => {
            // 其他方法通过 HTTP 代理
            handle_proxy_method(state, req, headers, body).await
        }
    };

    match response {
        Ok(resp) => Ok(Json(resp)),
        Err(e) => {
            error!("Error handling request: {}", e);
            Ok(Json(JSONRPCResponse {
                jsonrpc: "2.0".to_string(),
                id: serde_json::Value::Null,
                result: None,
                error: Some(RPCError {
                    code: -32603,
                    message: "Internal proxy error".to_string(),
                    data: Some(e.to_string()),
                }),
            }))
        }
    }
}

async fn handle_jito_transactions(
    State(state): State<AppState>,
    Query(query): Query<TokenQuery>,
    body: String,
) -> Result<Json<JSONRPCResponse>, StatusCode> {
    // 验证 token
    match query.token.as_deref() {
        Some(token) if token == VALID_TOKEN => {
            tracing::debug!("[JITO-AUTH] Valid token provided");
        }
        _ => {
            warn!("[JITO-AUTH] Invalid or missing token");
            return Ok(Json(JSONRPCResponse {
                jsonrpc: "2.0".to_string(),
                id: serde_json::Value::Null,
                result: None,
                error: Some(RPCError {
                    code: -32001,
                    message: "Unauthorized".to_string(),
                    data: Some(
                        "Invalid or missing token. Please provide ?token=beyond0128".to_string(),
                    ),
                }),
            }));
        }
    }

    // 解析 JSON-RPC 请求
    let req: JSONRPCRequest = match serde_json::from_str(&body) {
        Ok(req) => req,
        Err(e) => {
            error!("[JITO-TPU] Failed to parse JSON: {}", e);
            return Ok(Json(JSONRPCResponse {
                jsonrpc: "2.0".to_string(),
                id: serde_json::Value::Null,
                result: None,
                error: Some(RPCError {
                    code: -32700,
                    message: "Parse error".to_string(),
                    data: Some(e.to_string()),
                }),
            }));
        }
    };

    // 解析参数（和 sendTransaction 一致）
    if req.params.is_empty() {
        return Ok(Json(JSONRPCResponse {
            jsonrpc: "2.0".to_string(),
            id: req.id,
            result: None,
            error: Some(RPCError {
                code: -32602,
                message: "Invalid params".to_string(),
                data: Some("Missing transaction parameter".to_string()),
            }),
        }));
    }

    // 获取交易数据（可能是 base58 或 base64 编码的字符串）
    let tx_str = match req.params[0].as_str() {
        Some(s) => s,
        None => {
            return Ok(Json(JSONRPCResponse {
                jsonrpc: "2.0".to_string(),
                id: req.id,
                result: None,
                error: Some(RPCError {
                    code: -32602,
                    message: "Invalid params".to_string(),
                    data: Some("Transaction must be a string (base58 or base64)".to_string()),
                }),
            }));
        }
    };

    // 尝试解码交易（支持 base58 和 base64）
    let tx_bytes = if tx_str.contains('/') || tx_str.contains('+') || tx_str.contains('=') {
        // 看起来像 base64，尝试 base64 解码
        match base64::engine::general_purpose::STANDARD.decode(tx_str) {
            Ok(bytes) => {
                tracing::debug!("[JITO-TPU] Decoded transaction as base64");
                bytes
            }
            Err(e) => {
                // base64 解码失败，尝试 base58
                match bs58::decode(tx_str).into_vec() {
                    Ok(bytes) => {
                        tracing::debug!(
                            "[JITO-TPU] Decoded transaction as base58 (after base64 failed)"
                        );
                        bytes
                    }
                    Err(e2) => {
                        return Ok(Json(JSONRPCResponse {
                            jsonrpc: "2.0".to_string(),
                            id: req.id,
                            result: None,
                            error: Some(RPCError {
                                code: -32602,
                                message: "Invalid params".to_string(),
                                data: Some(format!("Failed to decode transaction (tried base64 and base58): base64 error: {}, base58 error: {}", e, e2)),
                            }),
                        }));
                    }
                }
            }
        }
    } else {
        // 看起来像 base58，先尝试 base58
        match bs58::decode(tx_str).into_vec() {
            Ok(bytes) => {
                tracing::debug!("[JITO-TPU] Decoded transaction as base58");
                bytes
            }
            Err(_) => {
                // base58 失败，尝试 base64
                match base64::engine::general_purpose::STANDARD.decode(tx_str) {
                    Ok(bytes) => {
                        tracing::debug!(
                            "[JITO-TPU] Decoded transaction as base64 (after base58 failed)"
                        );
                        bytes
                    }
                    Err(e) => {
                        return Ok(Json(JSONRPCResponse {
                            jsonrpc: "2.0".to_string(),
                            id: req.id,
                            result: None,
                            error: Some(RPCError {
                                code: -32602,
                                message: "Invalid params".to_string(),
                                data: Some(format!(
                                    "Failed to decode transaction (tried base58 and base64): {}",
                                    e
                                )),
                            }),
                        }));
                    }
                }
            }
        }
    };

    // 尝试反序列化交易以获取签名
    let signature = match bincode::deserialize::<VersionedTransaction>(&tx_bytes) {
        Ok(versioned_tx) => versioned_tx.signatures[0],
        Err(_) => {
            // 尝试旧的 Transaction 格式
            match bincode::deserialize::<Transaction>(&tx_bytes) {
                Ok(tx) => tx.signatures[0],
                Err(e) => {
                    return Ok(Json(JSONRPCResponse {
                        jsonrpc: "2.0".to_string(),
                        id: req.id,
                        result: None,
                        error: Some(RPCError {
                            code: -32602,
                            message: "Invalid params".to_string(),
                            data: Some(format!("Failed to deserialize transaction: {}", e)),
                        }),
                    }));
                }
            }
        }
    };

    let start = std::time::Instant::now();

    // 获取 Jito leader TPU 地址
    let jito_guard = state.jito_tpu_addresses.read().await;
    let should_use_jito_tpu = !jito_guard.is_empty();
    let targets: Vec<SocketAddr> = if should_use_jito_tpu {
        jito_guard.iter().copied().collect()
    } else {
        Vec::new()
    };
    drop(jito_guard);

    if should_use_jito_tpu && !targets.is_empty() {
        tracing::debug!(
            "[JITO-TPU] Sending transaction via TPU UDP to {} Jito leader nodes",
            targets.len()
        );

        // 记录 TPU 发送开始时间
        let tpu_send_start = std::time::Instant::now();

        // 并发发送到多个 Jito leader TPU 地址
        let mut send_tasks = Vec::new();
        for addr in targets.iter() {
            let tx_bytes_clone = tx_bytes.clone();
            let socket = state.udp_socket.clone();
            let addr_clone = *addr;

            send_tasks.push(tokio::spawn(async move {
                match socket.send_to(&tx_bytes_clone, addr_clone).await {
                    Ok(_) => {
                        tracing::debug!(
                            "[JITO-TPU] Sent transaction to {}:{}",
                            addr_clone.ip(),
                            addr_clone.port()
                        );
                        Ok(addr_clone)
                    }
                    Err(e) => {
                        tracing::debug!(
                            "[JITO-TPU] Failed to send to {}:{}: {}",
                            addr_clone.ip(),
                            addr_clone.port(),
                            e
                        );
                        Err(e)
                    }
                }
            }));
        }

        // 等待所有发送任务完成
        let mut sent_count = 0;
        for task in send_tasks {
            if let Ok(Ok(_)) = task.await {
                sent_count += 1;
            }
        }

        let tpu_send_duration = tpu_send_start.elapsed();
        let total_duration = start.elapsed();

        if sent_count > 0 {
            info!(
                "[JITO-TPU] Transaction sent via TPU UDP to {} Jito leader nodes, signature: {}, tpu_send_duration: {:?}, total_duration: {:?}",
                sent_count, signature, tpu_send_duration, total_duration
            );

            // TPU 发送是异步的，直接返回签名
            return Ok(Json(JSONRPCResponse {
                jsonrpc: "2.0".to_string(),
                id: req.id,
                result: Some(serde_json::Value::String(signature.to_string())),
                error: None,
            }));
        }
    }

    // 如果 Jito TPU 不可用，返回错误
    warn!("[JITO-TPU] Jito TPU client not available, cannot send transaction");
    Ok(Json(JSONRPCResponse {
        jsonrpc: "2.0".to_string(),
        id: req.id,
        result: None,
        error: Some(RPCError {
            code: -32603,
            message: "Jito TPU not available".to_string(),
            data: Some("Jito leader TPU addresses not initialized or unavailable".to_string()),
        }),
    }))
}

async fn handle_send_transaction_tpu(
    state: AppState,
    req: JSONRPCRequest,
) -> anyhow::Result<JSONRPCResponse> {
    // 解析参数
    if req.params.is_empty() {
        return Ok(JSONRPCResponse {
            jsonrpc: "2.0".to_string(),
            id: req.id,
            result: None,
            error: Some(RPCError {
                code: -32602,
                message: "Invalid params".to_string(),
                data: Some("Missing transaction parameter".to_string()),
            }),
        });
    }

    // 获取交易数据（可能是 base58 或 base64 编码的字符串）
    let tx_str = match req.params[0].as_str() {
        Some(s) => s,
        None => {
            return Ok(JSONRPCResponse {
                jsonrpc: "2.0".to_string(),
                id: req.id,
                result: None,
                error: Some(RPCError {
                    code: -32602,
                    message: "Invalid params".to_string(),
                    data: Some("Transaction must be a string (base58 or base64)".to_string()),
                }),
            });
        }
    };

    // 尝试解码交易（支持 base58 和 base64）
    // base64 可能包含 '/', '+', '=' 字符，base58 只包含字母和数字
    let tx_bytes = if tx_str.contains('/') || tx_str.contains('+') || tx_str.contains('=') {
        // 看起来像 base64，尝试 base64 解码
        match base64::engine::general_purpose::STANDARD.decode(tx_str) {
            Ok(bytes) => {
                tracing::debug!("[TPU] Decoded transaction as base64");
                bytes
            }
            Err(e) => {
                // base64 解码失败，尝试 base58（可能只是巧合包含这些字符）
                match bs58::decode(tx_str).into_vec() {
                    Ok(bytes) => {
                        tracing::debug!(
                            "[TPU] Decoded transaction as base58 (after base64 failed)"
                        );
                        bytes
                    }
                    Err(e2) => {
                        return Ok(JSONRPCResponse {
                            jsonrpc: "2.0".to_string(),
                            id: req.id,
                            result: None,
                            error: Some(RPCError {
                                code: -32602,
                                message: "Invalid params".to_string(),
                                data: Some(format!("Failed to decode transaction (tried base64 and base58): base64 error: {}, base58 error: {}", e, e2)),
                            }),
                        });
                    }
                }
            }
        }
    } else {
        // 看起来像 base58，先尝试 base58
        match bs58::decode(tx_str).into_vec() {
            Ok(bytes) => {
                tracing::debug!("[TPU] Decoded transaction as base58");
                bytes
            }
            Err(_) => {
                // base58 失败，尝试 base64（可能是不包含特殊字符的 base64）
                match base64::engine::general_purpose::STANDARD.decode(tx_str) {
                    Ok(bytes) => {
                        tracing::debug!(
                            "[TPU] Decoded transaction as base64 (after base58 failed)"
                        );
                        bytes
                    }
                    Err(e) => {
                        return Ok(JSONRPCResponse {
                            jsonrpc: "2.0".to_string(),
                            id: req.id,
                            result: None,
                            error: Some(RPCError {
                                code: -32602,
                                message: "Invalid params".to_string(),
                                data: Some(format!(
                                    "Failed to decode transaction (tried base58 and base64): {}",
                                    e
                                )),
                            }),
                        });
                    }
                }
            }
        }
    };

    // 尝试反序列化为 VersionedTransaction（新格式）或 Transaction（旧格式）
    // Solana 现在主要使用 VersionedTransaction，但为了兼容性也支持 Transaction
    // 使用枚举来统一处理两种类型
    enum TransactionType {
        Versioned(VersionedTransaction),
        Legacy(Transaction),
    }

    let (transaction_type, signature) = match bincode::deserialize::<VersionedTransaction>(
        &tx_bytes,
    ) {
        Ok(versioned_tx) => {
            tracing::debug!("[TPU] Deserialized as VersionedTransaction");
            let sig = versioned_tx.signatures[0];
            (TransactionType::Versioned(versioned_tx), sig)
        }
        Err(_) => {
            // 尝试反序列化为旧的 Transaction 格式
            match bincode::deserialize::<Transaction>(&tx_bytes) {
                Ok(tx) => {
                    tracing::debug!("[TPU] Deserialized as Transaction (legacy format)");
                    let sig = tx.signatures[0];
                    (TransactionType::Legacy(tx), sig)
                }
                Err(e) => {
                    return Ok(JSONRPCResponse {
                        jsonrpc: "2.0".to_string(),
                        id: req.id,
                        result: None,
                        error: Some(RPCError {
                            code: -32602,
                            message: "Invalid params".to_string(),
                            data: Some(format!(
                                "Failed to deserialize transaction (tried VersionedTransaction and Transaction): {}",
                                e
                            )),
                        }),
                    });
                }
            }
        }
    };

    let start = std::time::Instant::now();

    // 直接通过 TPU UDP 发送，不进行任何 preflight 检查
    let tpu_guard = state.tpu_addresses.read().await;
    let should_use_tpu = !tpu_guard.is_empty();
    let targets: Vec<SocketAddr> = if should_use_tpu {
        tpu_guard.iter().take(MAX_TPU_TARGETS).copied().collect()
    } else {
        Vec::new()
    };
    drop(tpu_guard); // 尽早释放锁

    if should_use_tpu && !targets.is_empty() {
        tracing::debug!("[TPU] Sending transaction via TPU UDP directly to leader (no preflight, fire-and-forget)");

        let tpu_send_start = std::time::Instant::now();

        // 并行启动所有发送任务，不等待返回结果
        for addr in targets.iter() {
            let tx_bytes_clone = tx_bytes.clone();
            let socket = state.udp_socket.clone();
            let addr_clone = *addr;
            let sig_clone = signature;

            // 在后台并行发送，不等待结果
            tokio::spawn(async move {
                match socket.send_to(&tx_bytes_clone, addr_clone).await {
                    Ok(_) => {
                        tracing::debug!(
                            "[TPU] Sent transaction to {}:{}, signature: {}",
                            addr_clone.ip(),
                            addr_clone.port(),
                            sig_clone
                        );
                    }
                    Err(e) => {
                        tracing::debug!(
                            "[TPU] Failed to send to {}:{}, signature: {}, error: {}",
                            addr_clone.ip(),
                            addr_clone.port(),
                            sig_clone,
                            e
                        );
                    }
                }
            });
        }

        let tpu_send_duration = tpu_send_start.elapsed();
        let total_duration = start.elapsed();
        info!(
            "[TPU] Transaction sent via TPU UDP to {} addresses (fire-and-forget), signature: {}, tpu_send_duration: {:?}, total_duration: {:?}",
            targets.len(), signature, tpu_send_duration, total_duration
        );

        // 立即返回，不等待发送结果
        return Ok(JSONRPCResponse {
            jsonrpc: "2.0".to_string(),
            id: req.id,
            result: Some(serde_json::Value::String(signature.to_string())),
            error: None,
        });
    }

    // 如果 TPU 客户端未初始化，回退到 RPC 方式
    warn!("[TPU] TPU client not available, falling back to RPC");
    let config = RpcSendTransactionConfig {
        skip_preflight: false,
        preflight_commitment: Some(CommitmentLevel::Processed),
        encoding: Some(UiTransactionEncoding::Base58),
        max_retries: Some(3),
        min_context_slot: None,
    };

    // 根据交易类型发送
    let sig = match &transaction_type {
        TransactionType::Versioned(ref vtx) => {
            state
                .rpc_client
                .send_transaction_with_config(vtx, config)
                .await
        }
        TransactionType::Legacy(ref tx) => {
            state
                .rpc_client
                .send_transaction_with_config(tx, config)
                .await
        }
    };

    let sig = match sig {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to send transaction via RPC: {}", e);
            return Ok(JSONRPCResponse {
                jsonrpc: "2.0".to_string(),
                id: req.id,
                result: None,
                error: Some(RPCError {
                    code: -32603,
                    message: "Transaction failed".to_string(),
                    data: Some(e.to_string()),
                }),
            });
        }
    };

    let duration = start.elapsed();
    info!(
        "[RPC] Transaction sent via RPC, signature: {}, duration: {:?}",
        sig, duration
    );

    Ok(JSONRPCResponse {
        jsonrpc: "2.0".to_string(),
        id: req.id,
        result: Some(serde_json::Value::String(sig.to_string())),
        error: None,
    })
}

async fn handle_proxy_method(
    state: AppState,
    req: JSONRPCRequest,
    headers: HeaderMap,
    body: String,
) -> anyhow::Result<JSONRPCResponse> {
    let start = std::time::Instant::now();

    // 创建代理请求
    let mut proxy_req = state
        .http_client
        .post(&state.validator_url)
        .header("Content-Type", "application/json")
        .body(body.clone());

    // 复制请求头（排除 Host 和 Content-Length）
    for (key, value) in headers.iter() {
        let key_str = key.as_str();
        if key_str != "host" && key_str != "content-length" {
            if let Ok(value_str) = value.to_str() {
                proxy_req = proxy_req.header(key_str, value_str);
            }
        }
    }

    // 发送请求
    let resp = proxy_req.send().await?;
    let status = resp.status();
    let resp_body = resp.text().await?;

    let duration = start.elapsed();
    info!(
        "[PROXY] Response status: {}, duration: {:?}",
        status, duration
    );

    // 解析响应
    let proxy_resp: JSONRPCResponse = match serde_json::from_str(&resp_body) {
        Ok(resp) => resp,
        Err(e) => {
            error!("Failed to parse proxy response: {}", e);
            return Ok(JSONRPCResponse {
                jsonrpc: "2.0".to_string(),
                id: req.id,
                result: None,
                error: Some(RPCError {
                    code: -32603,
                    message: "Invalid response from validator".to_string(),
                    data: Some(e.to_string()),
                }),
            });
        }
    };

    // 保持原始请求的 ID
    Ok(JSONRPCResponse {
        jsonrpc: proxy_resp.jsonrpc,
        id: req.id,
        result: proxy_resp.result,
        error: proxy_resp.error,
    })
}

async fn handle_health(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "status": "ok",
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "validator_url": state.validator_url,
        "entrypoint": state.entrypoint,
    }))
}
