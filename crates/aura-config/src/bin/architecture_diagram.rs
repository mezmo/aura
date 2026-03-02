use aura_config::load_config;
use std::env;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging
    tracing_subscriber::fmt().with_env_filter("debug").init();

    // Load environment variables
    if dotenv::dotenv().is_err() {
        println!("No .env file found, using system environment variables");
    }

    // Load configuration
    let config_path = env::args()
        .nth(1)
        .unwrap_or_else(|| "config.toml".to_string());
    let config = load_config(&config_path)?;

    println!("\n🏗️  RIG.RS ARCHITECTURE DIAGRAM FROM CONFIG");
    println!("═══════════════════════════════════════════════════════════");

    // Current Implementation (What's Actually Built)
    println!("\n📋 CURRENT IMPLEMENTATION (What's Actually Reified):");
    println!("┌─────────────────────────────────────────────────────────┐");
    println!("│                    🤖 SIMPLE AGENT                      │");
    println!("│                                                         │");

    let (provider, model) = match &config.llm {
        aura_config::config::LlmConfig::OpenAI { model, .. } => ("openai", model.clone()),
        aura_config::config::LlmConfig::Anthropic { model, .. } => ("anthropic", model.clone()),
        aura_config::config::LlmConfig::Bedrock { model, .. } => ("bedrock", model.clone()),
        aura_config::config::LlmConfig::Gemini { model, .. } => ("gemini", model.clone()),
        aura_config::config::LlmConfig::Ollama { model, .. } => ("ollama", model.clone()),
    };

    match provider {
        "openai" => {
            println!("│  ┌─────────────────────────────────────────────────┐    │");
            println!("│  │           🧠 OpenAI LLM Client                  │    │");
            println!("│  │                                                 │    │");
            println!("│  │  Provider: {provider}                        │    │");
            println!("│  │  Model: {model}                     │    │");
            println!(
                "│  │  System Prompt: {}...        │    │",
                config
                    .agent
                    .system_prompt
                    .chars()
                    .take(20)
                    .collect::<String>()
            );
            println!("│  └─────────────────────────────────────────────────┘    │");
        }
        "anthropic" => {
            println!("│  ┌─────────────────────────────────────────────────┐    │");
            println!("│  │          🧠 Anthropic LLM Client                │    │");
            println!("│  │                                                 │    │");
            println!("│  │  Provider: {provider}                      │    │");
            println!("│  │  Model: {model}                        │    │");
            println!(
                "│  │  System Prompt: {}...        │    │",
                config
                    .agent
                    .system_prompt
                    .chars()
                    .take(20)
                    .collect::<String>()
            );
            println!("│  └─────────────────────────────────────────────────┘    │");
        }
        "bedrock" => {
            println!("│  ┌─────────────────────────────────────────────────┐    │");
            println!("│  │          🌩️  AWS Bedrock LLM Client             │    │");
            println!("│  │                                                 │    │");
            println!("│  │  Provider: {provider}                      │    │");
            println!("│  │  Model: {model}       │    │");
            println!(
                "│  │  System Prompt: {}...        │    │",
                config
                    .agent
                    .system_prompt
                    .chars()
                    .take(20)
                    .collect::<String>()
            );
            println!("│  └─────────────────────────────────────────────────┘    │");
        }
        other => {
            println!("│  ┌─────────────────────────────────────────────────┐    │");
            println!("│  │            🧠 {other} LLM Client                 │    │");
            println!("│  │                                                 │    │");
            println!("│  │  ❌ NOT YET IMPLEMENTED                         │    │");
            println!("│  └─────────────────────────────────────────────────┘    │");
        }
    }
    println!("│                                                         │");
    println!("│  ❌ NO TOOLS CONNECTED                                  │");
    println!("│  ❌ NO MCP SERVERS CONNECTED                            │");
    println!("│  ❌ NO VECTOR STORE CONNECTED                           │");
    println!("└─────────────────────────────────────────────────────────┘");

    // Configuration Available (What Could Be Built)
    println!("\n🎯 TARGET ARCHITECTURE (What Config Defines):");
    println!("┌─────────────────────────────────────────────────────────┐");
    println!("│                  🤖 INTELLIGENT AGENT                   │");
    println!("│                                                         │");
    println!("│  ┌─────────────────────────────────────────────────┐    │");
    println!("│  │              🧠 LLM PROVIDER                    │    │");
    println!("│  │                                                 │    │");
    println!("│  │  Provider: {provider}                        │    │");
    println!("│  │  Model: {model}                     │    │");
    println!(
        "│  │  Temperature: {}                           │    │",
        config.agent.temperature.unwrap_or(0.7)
    );
    println!("│  └─────────────────────────────────────────────────┘    │");
    println!("│                          │                              │");
    println!("│                          ▼                              │");

    // Tools Section
    if let Some(ref tools) = config.tools {
        println!("│  ┌─────────────────────────────────────────────────┐    │");
        println!("│  │                🔧 TOOLS                         │    │");
        println!("│  │                                                 │    │");
        if tools.filesystem {
            println!("│  │  📁 Filesystem Tool (Rig built-in)             │    │");
        }
        for custom_tool in &tools.custom_tools {
            println!("│  │  🔨 Custom Tool: {custom_tool}                         │    │");
        }
        println!("│  └─────────────────────────────────────────────────┘    │");
        println!("│                          │                              │");
        println!("│                          ▼                              │");
    }

    // MCP Servers Section
    if let Some(ref mcp_config) = config.mcp {
        println!("│  ┌─────────────────────────────────────────────────┐    │");
        println!("│  │              🌐 MCP SERVERS                     │    │");
        println!("│  │                                                 │    │");

        for (name, server) in &mcp_config.servers {
            match server {
                aura_config::McpServerConfig::HttpStreamable { url, .. } => {
                    println!("│  │  🌊 {name} (HTTP Streamable)                   │    │");
                    println!(
                        "│  │     URL: {}              │    │",
                        if url.len() > 25 {
                            format!("{}...", &url[..25])
                        } else {
                            url.clone()
                        }
                    );
                }
                aura_config::McpServerConfig::Stdio { cmd, .. } => {
                    println!("│  │  💻 {name} (STDIO)                             │    │");
                    println!("│  │     Command: {cmd:?}                           │    │");
                }
            }
        }
        println!("│  └─────────────────────────────────────────────────┘    │");
        println!("│                          │                              │");
        println!("│                          ▼                              │");
    }

    // Vector Store Section
    println!("│  ┌─────────────────────────────────────────────────┐    │");
    println!("│  │              🗃️  VECTOR STORE                    │    │");
    println!("│  │                                                 │    │");
    if !config.vector_stores.is_empty() {
        let store = &config.vector_stores[0]; // Show first store
        println!(
            "│  │  Type: {} ({} stores)              │    │",
            store.store_type,
            config.vector_stores.len()
        );
        println!("│  │                                                 │    │");
        println!("│  │  ┌─────────────────────────────────────────┐    │    │");
        println!("│  │  │        🔤 EMBEDDING MODEL               │    │    │");
        println!("│  │  │                                         │    │    │");
        println!(
            "│  │  │  Provider: {}                │    │    │",
            store.embedding_model.provider
        );
        println!(
            "│  │  │  Model: {}    │    │    │",
            store.embedding_model.model
        );
    } else {
        println!("│  │  No vector stores configured               │    │");
        println!("│  │                                                 │    │");
        println!("│  │  ┌─────────────────────────────────────────┐    │    │");
        println!("│  │  │        🔤 NO EMBEDDING MODEL            │    │    │");
        println!("│  │  │                                         │    │    │");
        println!("│  │  │  Provider: N/A              │    │    │");
        println!("│  │  │  Model: N/A                 │    │    │");
    }
    println!("│  │  └─────────────────────────────────────────┘    │    │");
    println!("│  └─────────────────────────────────────────────────┘    │");
    println!("└─────────────────────────────────────────────────────────┘");

    // Configuration vs Implementation Gap
    println!("\n⚠️  IMPLEMENTATION GAPS:");
    println!("┌─────────────────────────────────────────────────────────┐");
    println!("│                  🚧 MISSING INTEGRATIONS                │");
    println!("│                                                         │");

    if config.mcp.is_some() {
        println!("│  🔴 MCP Server Integration                              │");
        println!("│     → Need to connect MCP servers to agent tools       │");
        println!("│     → Requires rig::agent.tool() integration           │");
        println!("│                                                         │");
    }

    println!("│  🔴 Vector Store Integration                            │");
    println!("│     → Need to create vector store from config          │");
    println!("│     → Need document ingestion pipeline                 │");
    println!("│     → Need to connect to agent for RAG queries         │");
    println!("│                                                         │");

    if let Some(ref tools) = config.tools {
        if tools.filesystem {
            println!("│  🔴 Filesystem Tool Integration                         │");
            println!("│     → Need to add filesystem tool to agent             │");
            println!("│     → Configure file access permissions                │");
            println!("│                                                         │");
        }
    }

    println!("└─────────────────────────────────────────────────────────┘");

    // Implementation Roadmap
    println!("\n🗺️  IMPLEMENTATION ROADMAP:");
    println!("┌─────────────────────────────────────────────────────────┐");
    println!("│                    📋 NEXT STEPS                        │");
    println!("│                                                         │");
    println!("│  1️⃣  MCP Server Integration                              │");
    println!("│      • Create MCP client connections from config       │");
    println!("│      • Add MCP tools to agent with .tool()             │");
    println!("│                                                         │");
    println!("│  2️⃣  Vector Store Implementation                         │");
    println!("│      • Create vector store from VectorStoreConfig      │");
    println!("│      • Implement document ingestion                    │");
    println!("│      • Add RAG retrieval to agent                      │");
    println!("│                                                         │");
    println!("│  3️⃣  Tools Integration                                   │");
    println!("│      • Add filesystem tools to agent                   │");
    println!("│      • Configure tool permissions and access           │");
    println!("│                                                         │");
    println!("│  4️⃣  Advanced Features                                   │");
    println!("│      • Multi-provider support                          │");
    println!("│      • Dynamic tool loading                            │");
    println!("│      • Configuration validation                        │");
    println!("└─────────────────────────────────────────────────────────┘");

    // Rig.rs API Usage
    println!("\n🔧 RIG.RS API INTEGRATION NEEDED:");
    println!("┌─────────────────────────────────────────────────────────┐");
    println!("│                   📚 RIG API CALLS                      │");
    println!("│                                                         │");
    println!("│  Current (Simple):                                      │");
    println!("│  ```rust                                                │");
    println!("│  let agent = client.agent(model).preamble(prompt).build()│");
    println!("│  ```                                                    │");
    println!("│                                                         │");
    println!("│  Target (Full Integration):                             │");
    println!("│  ```rust                                                │");
    println!("│  let agent = client.agent(model)                       │");
    println!("│      .preamble(system_prompt)                          │");
    println!("│      .tool(filesystem_tool)                            │");
    println!("│      .tool(mcp_tool_1)                                 │");
    println!("│      .tool(mcp_tool_2)                                 │");
    println!("│      .context(vector_store)                            │");
    println!("│      .build()                                          │");
    println!("│  ```                                                    │");
    println!("└─────────────────────────────────────────────────────────┘");

    println!("\n✅ Configuration parsing is complete and working!");
    println!("🎯 Next: Implement the missing integrations above\n");

    Ok(())
}
