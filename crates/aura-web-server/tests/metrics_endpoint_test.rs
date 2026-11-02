//! `/metrics` placement: on the API port by default, on its own listener
//! with `AURA_METRICS_LISTEN`. Spawns the real binary with a config that
//! needs no network, so this runs in the plain workspace test pass.

use std::net::TcpListener;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::{Child, Command};

const START_TIMEOUT: Duration = Duration::from_secs(30);

const CONFIG: &str = r#"
[agent]
name = "metrics-test"
system_prompt = "You are a test assistant."

[agent.llm]
provider = "openai"
api_key = "test"
model = "gpt-4o"
"#;

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

struct Server {
    child: Child,
    config_path: std::path::PathBuf,
}

impl Server {
    async fn start(port: u16, metrics_listen: Option<&str>) -> Self {
        let config_path =
            std::env::temp_dir().join(format!("aura-metrics-test-{}.toml", uuid::Uuid::new_v4()));
        std::fs::write(&config_path, CONFIG).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_aura-web-server"));
        command
            .env("CONFIG_PATH", &config_path)
            .env("HOST", "127.0.0.1")
            .env("PORT", port.to_string())
            .env("RUST_LOG", "warn")
            .env_remove("OTEL_EXPORTER_OTLP_ENDPOINT")
            .env_remove("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT")
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        if let Some(addr) = metrics_listen {
            command.env("AURA_METRICS_LISTEN", addr);
        }
        let child = command.spawn().expect("spawn aura-web-server");
        let server = Self { child, config_path };
        server.wait_healthy(port).await;
        server
    }

    async fn wait_healthy(&self, port: u16) {
        let client = reqwest::Client::new();
        let deadline = tokio::time::Instant::now() + START_TIMEOUT;
        loop {
            if let Ok(resp) = client
                .get(format!("http://127.0.0.1:{port}/health"))
                .send()
                .await
                && resp.status().is_success()
            {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "server on port {port} never answered /health"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn stop(mut self) {
        let _ = self.child.kill().await;
        let _ = std::fs::remove_file(&self.config_path);
    }
}

async fn get(port: u16, path: &str) -> reqwest::Response {
    reqwest::get(format!("http://127.0.0.1:{port}{path}"))
        .await
        .expect("request")
}

#[tokio::test]
async fn metrics_are_served_on_the_api_port_by_default() {
    let port = free_port();
    let server = Server::start(port, None).await;

    let response = get(port, "/metrics").await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()["content-type"],
        "text/plain; version=0.0.4"
    );
    let text = response.text().await.unwrap();
    assert!(text.contains("target_info{"), "{text}");
    // The /health probe that confirmed startup is already on the books.
    assert!(
        text.contains(r#"http_route="/health""#),
        "no request metric in:\n{text}"
    );

    server.stop().await;
}

#[tokio::test]
async fn metrics_listen_moves_the_endpoint_off_the_api_port() {
    let api_port = free_port();
    let metrics_port = free_port();
    let server = Server::start(api_port, Some(&format!("127.0.0.1:{metrics_port}"))).await;

    assert_eq!(get(api_port, "/metrics").await.status(), 404);
    let scraped = get(metrics_port, "/metrics").await;
    assert_eq!(scraped.status(), 200);
    assert!(scraped.text().await.unwrap().contains("target_info{"));
    assert_eq!(get(metrics_port, "/health").await.status(), 200);
    assert_eq!(get(metrics_port, "/v1/models").await.status(), 404);

    server.stop().await;
}
