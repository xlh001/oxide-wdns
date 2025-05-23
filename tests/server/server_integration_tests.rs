// tests/server/server_integration_tests.rs

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;
    use std::net::SocketAddr;
    use std::num::NonZeroU32;
    use reqwest::{Client, StatusCode, header::HeaderValue};
    use tokio::sync::oneshot;
    use hickory_proto::op::{Message, MessageType, OpCode};
    use hickory_proto::rr::{Name, RecordType};
    use tracing::{info, warn};
    use wiremock::{MockServer, Mock, matchers::{method, path, header}, ResponseTemplate};
    use tower_governor::{governor::GovernorConfigBuilder, GovernorLayer};
    use tower_governor::key_extractor::SmartIpKeyExtractor;
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD as BASE64_ENGINE};
    use tokio::time::sleep as tokio_sleep;
    use futures::future;
    
    // 项目内部导入
    use oxide_wdns::common::consts::{CONTENT_TYPE_DNS_MESSAGE, CONTENT_TYPE_DNS_JSON};
    use oxide_wdns::server::cache::DnsCache;
    use oxide_wdns::server::upstream::UpstreamManager;
    use oxide_wdns::server::routing::Router;
    use oxide_wdns::server::doh_handler::ServerState;
    use oxide_wdns::server::config::ServerConfig;
    
    
    
    // 导入公共测试工具
    use crate::server::mock_http_server::{find_free_port, create_test_query, create_test_response};
    
    // === 辅助函数 ===

    // 创建用于测试的配置
    fn build_test_config(port: u16, rate_limit_enabled: bool, cache_enabled: bool) -> ServerConfig {
        let config_str = format!(r#"
        http_server:
          listen_addr: "127.0.0.1:{}"
          timeout: 10
          rate_limit:
            enabled: {}
            per_ip_rate: 1
            per_ip_concurrent: 1
        dns_resolver:
          upstream:
            resolvers:
              - address: "8.8.8.8:53"
                protocol: udp
            query_timeout: 3
            enable_dnssec: false
          http_client:
            timeout: 5
            pool:
              idle_timeout: 60
              max_idle_connections: 20
            request:
              user_agent: "oxide-wdns-test/0.1.0"
          cache:
            enabled: {}
            size: 1000
            ttl:
              min: 10
              max: 300
              negative: 30
        "#, port, rate_limit_enabled, cache_enabled);
        
        serde_yaml::from_str(&config_str).expect("Failed to parse configuration")
    }

    // 创建服务器状态
    async fn create_server_state(port: u16, rate_limit_enabled: bool, cache_enabled: bool) -> ServerState {
        let config = build_test_config(port, rate_limit_enabled, cache_enabled);
        let router = Arc::new(Router::new(config.dns.routing.clone(), Some(Client::new())).await.unwrap());
        let http_client = Client::new();
        let upstream = Arc::new(UpstreamManager::new(Arc::new(config.clone()), http_client).await.unwrap());
        let cache = Arc::new(DnsCache::new(config.dns.cache.clone()));
        
        ServerState {
            config, 
            upstream, 
            cache, 
            router,
        }
    }

    // 创建一个DNS查询Message
    fn create_dns_query(domain: &str, record_type: RecordType) -> Message {
        let name = Name::from_ascii(domain).unwrap();
        let mut query = Message::new();
        query.set_id(1234)
             .set_message_type(MessageType::Query)
             .set_op_code(OpCode::Query)
             .add_query(hickory_proto::op::Query::query(name, record_type));
        query
    }

    // 在后台启动测试服务器
    async fn start_test_server(server_state: ServerState) -> (String, oneshot::Sender<()>) {
        let addr_str = server_state.config.http.listen_addr;
        let addr = format!("http://{}", addr_str);
        
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        
        let mut app = oxide_wdns::server::doh_handler::doh_routes(server_state.clone());
        
        if server_state.config.http.rate_limit.enabled {
            let config = &server_state.config.http.rate_limit;
            
            let burst_size_nz = NonZeroU32::new(config.per_ip_concurrent.max(1)).unwrap_or_else(|| {
                warn!("per_ip_concurrent configuration resulted in zero burst size, defaulting to 1");
                NonZeroU32::new(1).unwrap()
            });
            let burst_size_u32 = burst_size_nz.get();
            
            info!(
                per_second = config.per_ip_rate,
                burst_size = burst_size_u32,
                key_extractor = "SmartIpKeyExtractor",
                "Rate limiting enabled (using tower_governor in test setup)"
            );
            
            let governor_conf = Arc::new(
                GovernorConfigBuilder::default()
                    .key_extractor(SmartIpKeyExtractor)
                    .per_second(config.per_ip_rate.into()) 
                    .burst_size(burst_size_u32)
                    .error_handler(|_err| {
                        // 返回 429 Too Many Requests 响应
                        axum::response::Response::builder()
                            .status(StatusCode::TOO_MANY_REQUESTS)
                            .header("Retry-After", "5")
                            .body(axum::body::Body::from("Rate limit exceeded, please slow down and retry later."))
                            .unwrap()
                    }) 
                    .finish()
                    .unwrap(),
            );
            
            app = app.layer(GovernorLayer { config: governor_conf });
        }
        
        app = app
            .merge(oxide_wdns::server::health::health_routes())
            .merge(oxide_wdns::server::metrics::metrics_routes());
        
        let server_addr: SocketAddr = addr_str.to_string().parse().expect("Invalid listen address string"); 
        
        tokio::spawn(async move {
            let listener = tokio::net::TcpListener::bind(server_addr).await.unwrap();
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .unwrap();
        });
        
        tokio_sleep(Duration::from_millis(500)).await;
        
        (addr, shutdown_tx)
    }

    // === 使用 wiremock 实现的测试 ===
    
    // 新增一个使用 wiremock 测试上游 DoH 服务器的测试
    #[tokio::test]
    async fn test_server_with_mock_upstream() {
        // 启用 tracing 日志
        let _ = tracing_subscriber::fmt().with_env_filter("debug").try_init();
        info!("Starting test: test_server_with_mock_upstream");

        // 1. 启动上游 mock DoH 服务器
        let mock_upstream = MockServer::start().await;
        info!("Mock upstream DoH server started at: {}", mock_upstream.uri());
        
        // 配置 mock 服务器响应
        let response_message = create_test_response(
            &create_test_query("example.com", RecordType::A),
            std::net::Ipv4Addr::new(192, 168, 1, 1)
        );
        let response_bytes = response_message.to_vec().unwrap();
        
        Mock::given(method("POST"))
            .and(path("/dns-query"))
            .and(header("Content-Type", CONTENT_TYPE_DNS_MESSAGE))
            .respond_with(ResponseTemplate::new(200)
                .insert_header("Content-Type", CONTENT_TYPE_DNS_MESSAGE)
                .set_body_bytes(response_bytes.clone()))
            .mount(&mock_upstream)
            .await;
        
        // 2. 选择空闲端口并创建测试服务器配置
        let port = find_free_port().await;
        info!("Using port {}", port);
        
        // 自定义配置，使用 mock 上游服务器
        let mut config = build_test_config(port, false, false);
        config.dns.upstream.resolvers = vec![
            oxide_wdns::server::config::ResolverConfig {
                address: format!("{}/dns-query", mock_upstream.uri()),
                protocol: oxide_wdns::server::config::ResolverProtocol::Doh,
            }
        ];
        
        // 3. 创建服务器状态与组件
        let router = Arc::new(Router::new(config.dns.routing.clone(), Some(Client::new())).await.unwrap());
        let http_client = Client::new();
        let cache = Arc::new(DnsCache::new(config.dns.cache.clone()));
        let upstream = Arc::new(UpstreamManager::new(Arc::new(config.clone()), http_client).await.unwrap());
        
        let server_state = ServerState {
            config,
            upstream,
            cache,
            router,
        };
        
        // 4. 启动测试服务器
        let (server_addr, shutdown_tx) = start_test_server(server_state).await;
        info!("Test server started at: {}", server_addr);
        
        // 5. 创建测试查询
        let query = create_dns_query("example.com", RecordType::A);
        let query_bytes = query.to_vec().unwrap();
        
        // 6. 发送 DoH 请求到测试服务器 (服务器会转发到 mock 上游)
        let client = Client::new();
        let response = client
            .post(format!("{}/dns-query", server_addr))
            .header("Content-Type", CONTENT_TYPE_DNS_MESSAGE)
            .body(query_bytes)
            .send()
            .await
            .expect("Failed to send request to test server");
        
        // 7. 验证响应
        assert_eq!(response.status(), StatusCode::OK);
        let response_bytes = response.bytes().await.expect("Failed to read response body");
        let dns_response = Message::from_vec(&response_bytes).expect("Failed to parse DNS response");
        
        assert_eq!(dns_response.message_type(), MessageType::Response);
        assert!(!dns_response.answers().is_empty());
        
        // 8. 关闭服务器
        info!("Shutting down server...");
        let _ = shutdown_tx.send(());
        info!("Test completed: test_server_with_mock_upstream");
    }
    
    // 测试DNS分流功能，不同域名被路由到不同上游服务器
    #[tokio::test]
    async fn test_server_dns_routing_integration() {
        // 启用 tracing 日志
        let _ = tracing_subscriber::fmt().with_env_filter("debug").try_init();
        info!("Starting test: test_server_dns_routing_integration");

        // 1. 启动两个不同的 mock DoH 服务器，模拟不同的上游服务器组
        let mock_default = MockServer::start().await;
        let mock_cn = MockServer::start().await;
        let mock_secure = MockServer::start().await;
        info!("Mock upstream DoH servers started at: default={}, cn={}, secure={}", 
              mock_default.uri(), mock_cn.uri(), mock_secure.uri());
        
        // 为每个模拟服务器配置不同的响应IP，用于区分请求路由到了哪个上游
        // 创建查询和响应示例，用于后面的测试验证
        let _default_query = create_test_query("example.com", RecordType::A);
        let default_ip = std::net::Ipv4Addr::new(192, 168, 0, 1); // 默认组IP
        
        let _cn_query = create_test_query("example.cn", RecordType::A);
        let cn_ip = std::net::Ipv4Addr::new(192, 168, 0, 2); // CN组IP
        
        let _secure_query = create_test_query("secure.example.com", RecordType::A);
        let secure_ip = std::net::Ipv4Addr::new(192, 168, 0, 3); // 安全组IP
        
        // 通用响应处理函数 - 每个服务器总是返回固定IP，不管查询什么域名
        async fn setup_mock_upstream(mock_server: &MockServer, test_ip: std::net::Ipv4Addr) {
            Mock::given(method("POST"))
                .and(path("/dns-query"))
                .and(header("Content-Type", CONTENT_TYPE_DNS_MESSAGE))
                .respond_with(move |req: &wiremock::Request| {
                    // 解析DNS请求
                    let body = req.body.clone();
                    let query = Message::from_vec(&body).expect("Invalid DNS query");
                    
                    // 创建响应
                    let response = create_test_response(&query, test_ip);
                    let response_bytes = response.to_vec().unwrap();
                    
                    ResponseTemplate::new(200)
                        .insert_header("Content-Type", CONTENT_TYPE_DNS_MESSAGE)
                        .set_body_bytes(response_bytes)
                })
                .mount(mock_server)
                .await;
        }
        
        // 配置每个模拟服务器
        setup_mock_upstream(&mock_default, default_ip).await;
        setup_mock_upstream(&mock_cn, cn_ip).await;
        setup_mock_upstream(&mock_secure, secure_ip).await;
        
        // 2. 选择空闲端口并创建分流配置
        let port = find_free_port().await;
        info!("Using port {}", port);
        
        // 创建包含分流配置的测试配置
        let config_str = format!(r#"
        http_server:
          listen_addr: "127.0.0.1:{}"
          timeout: 10
          rate_limit:
            enabled: false
        dns_resolver:
          upstream:
            resolvers:
              - address: "{}/dns-query"
                protocol: doh
            query_timeout: 3
            enable_dnssec: false
          http_client:
            timeout: 5
            pool:
              idle_timeout: 60
              max_idle_connections: 20
            request:
              user_agent: "oxide-wdns-test/0.1.0"
          cache:
            enabled: false
          routing:
            enabled: true
            upstream_groups:
              - name: "cn_group"
                resolvers:
                  - address: "{}/dns-query"
                    protocol: doh
              - name: "secure_group"
                resolvers:
                  - address: "{}/dns-query"
                    protocol: doh
            rules:
              - match:
                  type: regex
                  values: [".*\\.cn$", ".*\\.com\\.cn$"]
                upstream_group: "cn_group"
              - match:
                  type: exact
                  values: ["secure.example.com"]
                upstream_group: "secure_group"
              - match:
                  type: exact
                  values: ["blocked.example.com"]
                upstream_group: "__blackhole__"
        "#, port, mock_default.uri(), mock_cn.uri(), mock_secure.uri());
        
        // 解析配置
        let config: ServerConfig = serde_yaml::from_str(&config_str).expect("Failed to parse configuration");
        
        // 3. 创建服务器状态并启动服务器
        info!("Creating server state with DNS routing configuration...");
        let router = Arc::new(Router::new(config.dns.routing.clone(), Some(Client::new())).await.unwrap());
        let http_client = Client::new();
        let cache = Arc::new(DnsCache::new(config.dns.cache.clone()));
        let upstream = Arc::new(UpstreamManager::new(Arc::new(config.clone()), http_client).await.unwrap());
        
        let server_state = ServerState {
            config,
            upstream,
            cache,
            router,
        };
        
        // 启动服务器
        info!("Starting test server with DNS routing...");
        let (server_addr, shutdown_tx) = start_test_server(server_state).await;
        
        // 4. 创建HTTP客户端
        let client = Client::new();
        
        // 5. 测试不同类型的域名请求
        
        // a. 测试全局默认上游 (example.com)
        info!("Testing default upstream routing (example.com)...");
        let response = query_doh(&client, &server_addr, "example.com", RecordType::A).await;
        let addresses = extract_ip_addresses(&response);
        assert_eq!(addresses, vec!["192.168.0.1"], "example.com should use default upstream group");
        
        // b. 测试CN组上游 (example.cn)
        info!("Testing CN group routing (example.cn)...");
        let response = query_doh(&client, &server_addr, "example.cn", RecordType::A).await;
        let addresses = extract_ip_addresses(&response);
        assert_eq!(addresses, vec!["192.168.0.2"], "example.cn should use CN upstream group");
        
        // c. 测试安全组上游 (secure.example.com)
        info!("Testing secure group routing (secure.example.com)...");
        let response = query_doh(&client, &server_addr, "secure.example.com", RecordType::A).await;
        let addresses = extract_ip_addresses(&response);
        assert_eq!(addresses, vec!["192.168.0.3"], "secure.example.com should use secure upstream group");
        
        // d. 测试黑洞组 (blocked.example.com)
        info!("Testing blackhole routing (blocked.example.com)...");
        let response = query_doh(&client, &server_addr, "blocked.example.com", RecordType::A).await;
        assert_eq!(response.response_code(), hickory_proto::op::ResponseCode::NXDomain, 
                   "blocked.example.com should be blackholed (return NXDomain)");
        
        // 关闭服务器
        let _ = shutdown_tx.send(());
        info!("Test completed: test_server_dns_routing_integration");
    }
    
    // 辅助函数：发送DoH查询
    async fn query_doh(client: &Client, server_addr: &str, domain: &str, record_type: RecordType) -> Message {
        // 创建DNS查询消息
        let query = create_test_query(domain, record_type);
        let query_bytes = query.to_vec().unwrap();
        
        // 发送DoH POST请求
        let response = client.post(format!("{}/dns-query", server_addr))
            .header(reqwest::header::CONTENT_TYPE, CONTENT_TYPE_DNS_MESSAGE)
            .body(query_bytes)
            .send()
            .await
            .expect("DoH request failed");
        
        assert_eq!(response.status(), StatusCode::OK, "DoH request should return OK status");
        
        // 解析响应
        let response_bytes = response.bytes().await.expect("Failed to get response content");
        Message::from_vec(&response_bytes).expect("Failed to parse DNS response")
    }
    
    // 辅助函数：从DNS响应中提取IP地址
    fn extract_ip_addresses(message: &Message) -> Vec<String> {
        message.answers()
            .iter()
            .filter_map(|answer| {
                if let Some(hickory_proto::rr::RData::A(ipv4)) = answer.data() {
                    Some(ipv4.to_string())
                } else {
                    None
                }
            })
            .collect()
    }

    // === 其余原始测试保持不变 ===
    
    #[tokio::test]
    async fn test_server_starts_and_responds_to_health_check() {
        // 启用 tracing 日志
        let _ = tracing_subscriber::fmt().with_env_filter("debug").try_init();
        info!("Starting test: test_server_starts_and_responds_to_health_check");

        // 1. 选择空闲端口
        let port = find_free_port().await;
        info!("Using port {}", port);

        // 2. 配置服务器
        let server_state = create_server_state(port, false, false).await;

        // 3. 启动服务器
        info!("Starting test server...");
        let (server_addr, shutdown_tx) = start_test_server(server_state).await;
        info!("Server started at address: {}", server_addr);

        // 4. 创建HTTP客户端
        let client = Client::new();

        // 5. 发送健康检查请求
        info!("Sending health check request...");
        let response = client
            .get(format!("{}/health", server_addr).parse::<reqwest::Url>().unwrap())
            .send()
            .await
            .expect("Health check request failed");
        info!("Health check response status: {}", response.status());

        // 6. 断言：收到 200 OK 响应
        assert_eq!(response.status(), StatusCode::OK);

        // 7. 清理：关闭服务器
        info!("Shutting down server...");
        let _ = shutdown_tx.send(());
        info!("Test completed: test_server_starts_and_responds_to_health_check");
    }

    #[tokio::test]
    async fn test_server_handles_basic_doh_query() {
        // 启用 tracing 日志
        let _ = tracing_subscriber::fmt().with_env_filter("debug").try_init();
        info!("Starting test: test_server_handles_basic_doh_query");

        // 1. 选择空闲端口
        let port = find_free_port().await;
        info!("Using port {}", port);

        // 2. 配置服务器
        let server_state = create_server_state(port, false, false).await;

        // 3. 启动服务器
        info!("Starting test server...");
        let (server_addr, shutdown_tx) = start_test_server(server_state).await;
        info!("Server started at address: {}", server_addr);

        // 4. 准备DNS查询
        let query = create_dns_query("example.com", RecordType::A);
        let query_bytes = query.to_vec().unwrap();
        info!("Prepared DNS query for example.com (A)");

        // 5. 创建Reqwest HTTP客户端 (注意: 这里使用 reqwest)
        let client = Client::new();

        // 6. 发送DoH POST请求
        info!("Sending DoH POST request...");
        let response = client
            .post(format!("{}/dns-query", server_addr))
            .header(reqwest::header::CONTENT_TYPE, CONTENT_TYPE_DNS_MESSAGE)
            .body(query_bytes)
            .send()
            .await
            .expect("DoH POST request failed");
        let status = response.status();
        info!("DoH POST response status: {}", status);

        // 7. 断言：收到 200 OK 响应
        assert_eq!(status, StatusCode::OK);

        // 8. 断言：响应体是有效的 DNS 消息
        let response_bytes = response.bytes().await.expect("Failed to read response body");
        info!("Received response body ({} bytes)", response_bytes.len());
        let dns_response = Message::from_vec(&response_bytes).expect("Invalid DNS message format in response");
        info!("Successfully parsed DNS response");
        assert_eq!(dns_response.message_type(), MessageType::Response);

        // 9. 清理：关闭服务器
        info!("Shutting down server...");
        let _ = shutdown_tx.send(());
        info!("Test completed: test_server_handles_basic_doh_query");
    }

    #[tokio::test]
    async fn test_server_metrics_endpoint_works() {
        // 启用 tracing 日志
        let _ = tracing_subscriber::fmt().with_env_filter("debug").try_init();
        info!("Starting test: test_server_metrics_endpoint_works");

        // 1. 选择空闲端口
        let port = find_free_port().await;
        info!("Using port {}", port);

        // 2. 配置服务器
        let server_state = create_server_state(port, false, false).await;

        // 3. 启动服务器
        info!("Starting test server...");
        let (server_addr, shutdown_tx) = start_test_server(server_state).await;
        info!("Server started at address: {}", server_addr);

        // 等待指标系统初始化
        tokio_sleep(Duration::from_millis(500)).await;

        // 4. 创建Reqwest HTTP客户端
        let client = Client::new();

        // 5. 发送指标请求
        info!("Sending metrics request...");
        let metrics_response = client
            .get(format!("{}/metrics", server_addr)) // Changed to /metrics
            .send()
            .await
            .expect("Metrics request failed");
        let status = metrics_response.status();
        info!("Metrics response status: {}", status);

        // 6. 断言：收到 200 OK 响应
        assert_eq!(status, StatusCode::OK);

        // 7. 断言：响应体内容不为空，并且包含 Prometheus 格式的指标
        let metrics_text = metrics_response.text().await.unwrap();
        info!("Received metrics response body ({} bytes)", metrics_text.len());
        assert!(
            !metrics_text.is_empty() && (
                metrics_text.contains("dns_") || 
                metrics_text.contains("doh_") || 
                metrics_text.contains("cache_") || 
                metrics_text.contains("upstream_")
            ),
            "Response should contain valid metrics data"
        );
        info!("Metrics response body contains expected format.");

        // 8. 清理：关闭服务器
        info!("Shutting down server...");
        let _ = shutdown_tx.send(());
        info!("Test completed: test_server_metrics_endpoint_works");
    }

    // 在测试环境中，由于速率限制依赖于底层中间件
    #[tokio::test]
    async fn test_server_applies_rate_limit() {
        // 启用 tracing 日志，帮助排查问题
        let _ = tracing_subscriber::fmt()
            .with_env_filter("debug,tower_governor=debug,hyper=info,reqwest=info")
            .try_init();
        
        // 1. 选择空闲端口，创建配置，启用较低的速率限制（每秒1个请求，并发1个 -> burst_size 1）
        let port = find_free_port().await;
        info!("Using port {}", port);
        let server_state = create_server_state(port, true, false).await;
        
        // 2. 启动服务器
        let (server_addr, shutdown_tx) = start_test_server(server_state).await;
        info!("Server started at address: {}", server_addr);
        
        // 等待服务器完全启动并初始化速率限制器
        info!("Waiting for server and rate limiter initialization...");
        tokio_sleep(Duration::from_millis(1000)).await;
        
        // 预热 - 发送请求和一个 GET 请求到健康端点
        info!("Warming up server...");
        let warmup_client = Client::new();
        let warmup_response = warmup_client.get(format!("{}/health", server_addr))
            .send()
            .await
            .expect("Warmup health check failed");
        info!("Warmup health check response: {:?}", warmup_response.status());
        tokio_sleep(Duration::from_secs(1)).await;
        
        // 3. 准备DNS查询
        let query = create_dns_query("example.net", RecordType::A);
        let query_bytes = query.to_vec().unwrap();
        
        // 创建单个客户端实例，确保所有请求都来自同一个"IP"
        // 禁用重定向和重试机制，确保测试稳定
        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap();
        
        // 4. 使用 tokio 并发发送多个请求
        info!("Sending concurrent requests to test rate limiting...");
        
        // 定义要发送的请求数量（大于速率限制阈值）
        const REQUEST_COUNT: usize = 10;
        
        // 创建一个用于存储所有响应状态码的向量
        let mut status_codes = Vec::new();
        
        // 使用 join_all 并发发送多个请求
        let tasks: Vec<_> = (0..REQUEST_COUNT).map(|i| {
            let client = client.clone();
            let server_addr = server_addr.clone();
            let query_bytes = query_bytes.clone();
            
            tokio::spawn(async move {
                info!("Sending request #{}", i);
                match client.post(format!("{}/dns-query", server_addr))
                    .header(reqwest::header::CONTENT_TYPE, CONTENT_TYPE_DNS_MESSAGE)
                    .body(query_bytes)
                    .send()
                    .await
                {
                    Ok(response) => {
                        let status = response.status();
                        info!("Request #{} status: {:?}", i, status);
                        status
                    },
                    Err(e) => {
                        warn!("Request #{} failed: {:?}", i, e);
                        StatusCode::INTERNAL_SERVER_ERROR
                    }
                }
            })
        }).collect();
        
        // 等待所有请求完成
        let results = future::join_all(tasks).await;
        
        // 收集所有响应状态码
        status_codes.extend(results.into_iter().flatten());
        
        info!("Received status codes: {:?}", status_codes);
        
        // 断言：至少有一个请求被速率限制（状态码为 429）
        assert!(status_codes.contains(&StatusCode::TOO_MANY_REQUESTS), 
                "At least one request should be rate limited (status code 429)");
        
        // 清理：关闭服务器
        info!("Test completed, shutting down server");
        let _ = shutdown_tx.send(());
    }

    #[tokio::test]
    async fn test_server_cache_integration() {
        // 启用 tracing 日志
        let _ = tracing_subscriber::fmt().with_env_filter("debug").try_init();
        info!("Starting test: test_server_cache_integration");

        // 1. 配置并启动服务器，启用缓存
        let port = find_free_port().await;
        info!("Using port {}", port);
        let server_state = create_server_state(port, false, true).await; // cache_enabled: true
        info!("Server configured with cache enabled.");

        // 2. 启动服务器
        info!("Starting test server...");
        let (server_addr, shutdown_tx) = start_test_server(server_state).await;
        info!("Server started at address: {}", server_addr);

        // 3. 准备DNS查询
        let query = create_dns_query("example.com", RecordType::A);
        let query_bytes = query.to_vec().unwrap();
        info!("Prepared DNS query for example.com (A)");

        let client = Client::new();

        // 4. 发送第一个请求
        info!("Sending first DoH request...");
        let first_response = client
            .post(format!("{}/dns-query", server_addr))
            .header(reqwest::header::CONTENT_TYPE, CONTENT_TYPE_DNS_MESSAGE)
            .body(query_bytes.clone())
            .send()
            .await
            .expect("First DoH request failed");
        let first_status = first_response.status();
        info!("First DoH response status: {}", first_status);

        // 确保第一个请求成功
        assert_eq!(first_status, StatusCode::OK);
        let first_body = first_response
            .bytes()
            .await
            .expect("Failed to read first response body");
        info!("Received first response body ({} bytes)", first_body.len());
        let first_dns_message =
            Message::from_vec(&first_body).expect("Failed to parse first DNS response");
        info!("Parsed first DNS response, ID: {}", first_dns_message.id());

        // 短暂等待，确保缓存有机会生效（理论上不需要，但增加稳定性）
        tokio_sleep(Duration::from_millis(100)).await;

        // 5. 立即再次发送相同的DoH查询
        info!("Sending second (cached) DoH request...");
        let second_response = client
            .post(format!("{}/dns-query", server_addr))
            .header(reqwest::header::CONTENT_TYPE, CONTENT_TYPE_DNS_MESSAGE)
            .body(query_bytes)
            .send()
            .await
            .expect("Second DoH request failed");
        let second_status = second_response.status();
        info!("Second DoH response status: {}", second_status);

        // 确保第二个请求也成功
        assert_eq!(second_status, StatusCode::OK);

        // 获取第二个响应体
        let second_body = second_response
            .bytes()
            .await
            .expect("Failed to read second response body");
        info!("Received second response body ({} bytes)", second_body.len());

        // 确保两个响应的消息ID相同（因为缓存会保留原始消息）
        let second_dns_message =
            Message::from_vec(&second_body).expect("Failed to parse second DNS response");
        info!("Parsed second DNS response, ID: {}", second_dns_message.id());

        // 比较消息ID，如果相同则表明是缓存的响应
        assert_eq!(
            first_dns_message.id(),
            second_dns_message.id(),
            "Cache should return the same DNS message ID"
        );
        info!("Verified that response was served from cache (matching IDs).");

        // 6. 清理：关闭服务器
        info!("Shutting down server...");
        let _ = shutdown_tx.send(());
        info!("Test completed: test_server_cache_integration");
    }
    
    #[tokio::test]
    async fn test_server_doh_get_request() {
        // 启用 tracing 日志
        let _ = tracing_subscriber::fmt().with_env_filter("debug").try_init();
        info!("Starting test: test_server_doh_get_request");

        // 1. 选择空闲端口
        let port = find_free_port().await;
        info!("Using port {}", port);

        // 2. 配置服务器
        let server_state = create_server_state(port, false, false).await;

        // 3. 启动服务器
        info!("Starting test server...");
        let (server_addr, shutdown_tx) = start_test_server(server_state).await;
        info!("Server started at address: {}", server_addr);

        // 4. 构造一个简单的 DNS 查询
        let query = create_dns_query("example.com", RecordType::A);
        let query_bytes = query.to_vec().unwrap();
        info!("Prepared DNS query for example.com (A)");

        // 5. 将查询编码为Base64url
        let encoded_query = BASE64_ENGINE.encode(&query_bytes);
        info!(
            "Encoded query (Base64url): {}...",
            &encoded_query[..std::cmp::min(encoded_query.len(), 20)]
        ); // Log prefix

        // 6. 创建Reqwest HTTP客户端
        let client = Client::new();

        // 7. 发送DoH GET请求
        let get_url = format!("{}/dns-query?dns={}", server_addr, encoded_query);
        info!("Sending DoH GET request to: {}", get_url);
        let response = client
            .get(&get_url) // 使用引用
            .send()
            .await
            .expect("DoH GET request failed");
        let status = response.status();
        info!("DoH GET response status: {}", status);

        // 8. 断言：收到 200 OK 响应
        assert_eq!(status, StatusCode::OK);

        // 9. 断言：响应体是有效的 DNS 消息
        let response_bytes = response.bytes().await.expect("Failed to read response body");
        info!("Received response body ({} bytes)", response_bytes.len());
        let dns_response = Message::from_vec(&response_bytes).expect("Invalid DNS message format in response");
        info!("Successfully parsed DNS response from GET request");
        assert_eq!(dns_response.message_type(), MessageType::Response);

        // 10. 清理：关闭服务器
        info!("Shutting down server...");
        let _ = shutdown_tx.send(());
        info!("Test completed: test_server_doh_get_request");
    }
    
    #[tokio::test]
    async fn test_server_rejects_invalid_content_type() {
        // 启用 tracing 日志
        let _ = tracing_subscriber::fmt().with_env_filter("debug").try_init();
        info!("Starting test: test_server_rejects_invalid_content_type");

        // 1. 选择空闲端口
        let port = find_free_port().await;
        info!("Using port {}", port);

        // 2. 配置服务器
        let server_state = create_server_state(port, false, false).await;

        // 3. 启动服务器
        info!("Starting test server...");
        let (server_addr, shutdown_tx) = start_test_server(server_state).await;
        info!("Server started at address: {}", server_addr);

        // 4. 准备一些假的请求体
        let fake_body = b"this is not a dns message".to_vec();

        // 5. 创建Reqwest HTTP客户端
        let client = Client::new();

        // 6. 发送带有错误 Content-Type 的 POST 请求
        info!(
            "Sending DoH POST request with invalid Content-Type: {}",
            CONTENT_TYPE_DNS_JSON
        );
        let response = client
            .post(format!("{}/dns-query", server_addr))
            .header("Content-Type", HeaderValue::from_static(CONTENT_TYPE_DNS_JSON)) // 修复 Content-Type 设置
            .body(fake_body)
            .send()
            .await
            .expect("Request with invalid content type failed");
        let status = response.status();
        info!("Response status for invalid Content-Type: {}", status);

        // 7. 断言：收到 415 Unsupported Media Type 响应
        assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);

        // 8. 清理：关闭服务器
        info!("Shutting down server...");
        let _ = shutdown_tx.send(());
        info!("Test completed: test_server_rejects_invalid_content_type");
    }
    
    #[tokio::test]
    async fn test_server_handles_different_query_types() {
        // 启用 tracing 日志
        let _ = tracing_subscriber::fmt().with_env_filter("debug").try_init();
        info!("Starting test: test_server_handles_different_query_types");

        // 1. 选择空闲端口
        let port = find_free_port().await;
        info!("Using port {}", port);

        // 2. 配置服务器
        let server_state = create_server_state(port, false, false).await;

        // 3. 启动服务器
        info!("Starting test server...");
        let (server_addr, shutdown_tx) = start_test_server(server_state).await;
        info!("Server started at address: {}", server_addr);

        // 4. 创建HTTP客户端
        let client = Client::new();
        
        // 5. 测试不同的查询类型
        for record_type in [RecordType::A, RecordType::AAAA, RecordType::MX, RecordType::TXT] {
            // 构造DNS查询
            let query = create_dns_query("example.com", record_type);
            let query_bytes = query.to_vec().unwrap();
            
            // 发送请求
            let response = client.post(format!("{}/dns-query", server_addr))
                .header(reqwest::header::CONTENT_TYPE, CONTENT_TYPE_DNS_MESSAGE)
                .body(query_bytes)
                .send()
                .await
                .unwrap_or_else(|_| panic!("{:?} query request failed", record_type));
            
            // 断言响应成功
            assert_eq!(response.status(), StatusCode::OK);
            
            // 解析DNS响应
            let response_bytes = response.bytes().await.unwrap();
            let dns_response = Message::from_vec(&response_bytes).expect("Failed to parse DNS response");
            
            // 验证响应类型
            assert_eq!(dns_response.message_type(), MessageType::Response);
        }
        
        // 6. 清理：关闭服务器
        info!("Shutting down server...");
        let _ = shutdown_tx.send(());
        info!("Test completed: test_server_handles_different_query_types");
    }
} 