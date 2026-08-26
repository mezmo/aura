//! Spawn and control an `aura webserver` process for the durability harness.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::{Child, Command};

/// Which session-store backend the harness server should use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionStoreBackend {
    /// File-backed approvals and artifacts; durable parking capability is
    /// absent by design in this board.
    File,
    /// Redis/Valkey-backed approvals and event bus. Durable parking is also
    /// absent in this board, so the run fails closed at the park gate.
    Redis,
}

/// Configuration for a harness server instance.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// Base URL of the stub LLM.
    pub llm_base_url: String,
    /// URL of the mock MCP server.
    pub mcp_url: String,
    /// Filesystem root for the file session store and orchestration artifacts.
    pub memory_dir: PathBuf,
    /// TCP port to bind to.
    pub port: u16,
    /// Session-store backend.
    pub backend: SessionStoreBackend,
    /// Redis URL when [`SessionStoreBackend::Redis`] is selected.
    pub redis_url: Option<String>,
}

impl ServerConfig {
    /// Render the agent TOML config that the server will load.
    pub fn to_toml(&self) -> String {
        format!(
            r#"
[agent]
name = "Durability Orchestrator"
alias = "durability"
system_prompt = """
You are a coordinator for the durability harness. You always create a plan
with one worker that calls the gated mock_tool.
"""
turn_depth = 5

[agent.llm]
provider = "ollama"
base_url = "{llm_base_url}"
model = "aura-stub"
temperature = 0.0

[mcp]
sanitize_schemas = true

[mcp.servers.mock]
transport = "http_streamable"
url = "{mcp_url}"
headers = {{}}
description = "Mock MCP server with a gated tool"

[orchestration]
enabled = true
max_planning_cycles = 2
allow_direct_answers = false
allow_clarification = false
tools_in_planning = "summary"

[orchestration.timeouts]
per_call_timeout_secs = 60
stream_inactivity_timeout_secs = 45

[orchestration.artifacts]
memory_dir = "{memory_dir}"

[orchestration.worker.gated]
description = "Worker that exercises the HITL gate"
turn_depth = 5
mcp_filter = ["mock_tool"]
preamble = """
You are a worker whose only job is to call the mock_tool with the message
\"exercise the HITL gate\". Do not do anything else.
"""

[hitl]
require_approval = ["mock_tool"]

[hitl.route]
mode = "conversational"
timeout_secs = 300
"#,
            llm_base_url = self.llm_base_url,
            mcp_url = self.mcp_url,
            memory_dir = self.memory_dir.display(),
        )
    }
}

/// A running (or stopped) `aura webserver` child process.
pub struct AuraServerProcess {
    config: ServerConfig,
    config_path: PathBuf,
    child: Option<Child>,
    logs_dir: PathBuf,
}

impl AuraServerProcess {
    /// Build a fresh server instance. The process is not started until
    /// [`Self::start`] is called.
    pub fn new(config: ServerConfig) -> std::io::Result<Self> {
        let logs_dir = config.memory_dir.join("logs");
        std::fs::create_dir_all(&logs_dir)?;
        let config_path = config.memory_dir.join("agent.toml");
        std::fs::write(&config_path, config.to_toml())?;
        Ok(Self {
            config,
            config_path,
            child: None,
            logs_dir,
        })
    }

    /// Start the server and wait for `/health` to return 200.
    pub async fn start(&mut self) -> std::io::Result<()> {
        if self.child.is_some() {
            return Err(std::io::Error::other("server already started"));
        }

        let bin = find_server_binary().await?;
        let stdout_path = self.logs_dir.join("stdout.log");
        let stderr_path = self.logs_dir.join("stderr.log");
        let stdout = Stdio::from(std::fs::File::create(&stdout_path)?);
        let stderr = Stdio::from(std::fs::File::create(&stderr_path)?);

        let mut cmd = Command::new(&bin);
        cmd.arg("webserver")
            .arg("--config")
            .arg(&self.config_path)
            .arg("--host")
            .arg("127.0.0.1")
            .arg("--port")
            .arg(self.config.port.to_string())
            .arg("--aura-custom-events")
            .arg("true")
            .env("RUST_LOG", "warn,aura=info,aura_web_server=info");

        match self.config.backend {
            SessionStoreBackend::File => {
                cmd.env("AURA_SESSION_STORE", "file");
                cmd.env("AURA_SESSION_STORE_URL", &self.config.memory_dir);
            }
            SessionStoreBackend::Redis => {
                let redis_url = self
                    .config
                    .redis_url
                    .as_ref()
                    .expect("redis backend requires a redis_url");
                cmd.env("AURA_SESSION_STORE", "redis");
                cmd.env("AURA_SESSION_STORE_URL", redis_url);
            }
        }

        let mut child = cmd
            .stdout(stdout)
            .stderr(stderr)
            .kill_on_drop(true)
            .spawn()?;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let client = reqwest::Client::new();
        let health_url = format!("http://127.0.0.1:{}/health", self.config.port);

        loop {
            if tokio::time::Instant::now() >= deadline {
                let _ = child.start_kill();
                return Err(std::io::Error::other(format!(
                    "server did not become healthy within 30s (stdout: {}, stderr: {})",
                    stdout_path.display(),
                    stderr_path.display()
                )));
            }

            if let Ok(resp) = client
                .get(&health_url)
                .timeout(Duration::from_secs(2))
                .send()
                .await
            {
                if resp.status().is_success() {
                    self.child = Some(child);
                    return Ok(());
                }
            }

            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// Return the API base URL.
    pub fn api_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.config.port)
    }

    /// Return the memory directory.
    pub fn memory_dir(&self) -> &Path {
        &self.config.memory_dir
    }

    /// Stop the server with SIGTERM and wait for exit.
    pub async fn stop(&mut self) -> std::io::Result<()> {
        if let Some(mut child) = self.child.take() {
            child.start_kill()?;
            let _ = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;
        }
        Ok(())
    }

    /// Stop the current process and start a fresh one with the same config.
    pub async fn restart(&mut self) -> std::io::Result<()> {
        self.stop().await?;
        self.start().await
    }
}

async fn find_server_binary() -> std::io::Result<PathBuf> {
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_aura") {
        let p = PathBuf::from(path);
        if p.exists() {
            return Ok(p);
        }
    }

    let current = std::env::current_exe()?;
    let mut dir = current.parent();
    while let Some(d) = dir {
        let candidate = d.join("aura");
        if candidate.exists() {
            return Ok(candidate);
        }
        if d.ends_with("target") {
            // We have walked up to target/ without finding it; build it.
            break;
        }
        dir = d.parent();
    }

    // Fallback: build the binary so the harness can be run with a plain
    // `cargo test` invocation.
    let status = tokio::process::Command::new("cargo")
        .args([
            "build",
            "-p",
            "aura-cli",
            "--bin",
            "aura",
            "--features",
            "session-store-redis",
        ])
        .status()
        .await?;
    if !status.success() {
        return Err(std::io::Error::other("failed to build aura binary"));
    }

    let current = std::env::current_exe()?;
    let mut dir = current.parent();
    while let Some(d) = dir {
        let candidate = d.join("aura");
        if candidate.exists() {
            return Ok(candidate);
        }
        dir = d.parent();
    }

    Err(std::io::Error::other(
        "could not locate aura binary after building",
    ))
}
