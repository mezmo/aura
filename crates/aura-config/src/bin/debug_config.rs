use aura_config::load_config_from_str;

const TEST_CONFIG: &str = r#"
# Test configuration with environment variables set to test values
[llm]
provider = "openai"
api_key = "test_openai_key"
model = "gpt-4o-mini"

[mcp.servers.mezmo]
transport = "http_streamable"
url = "https://mcp.mezmo.com/mcp"
headers = { Authorization = "Bearer test_mezmo_key" }
description = "Mezmo MCP server for log analysis and monitoring"

[mcp.servers.bedrock_kb]
transport = "stdio"
cmd = ["uvx"]
args = ["awslabs.bedrock-kb-retrieval-mcp-server@latest"]
description = "AWS Bedrock Knowledge Base retrieval server for RAG capabilities"

[mcp.servers.bedrock_kb.env]
AWS_PROFILE = "test_profile"
AWS_REGION = "us-east-1"
FASTMCP_LOG_LEVEL = "ERROR"
KB_INCLUSION_TAG_KEY = "mcp_enabled"
BEDROCK_KB_RERANKING_ENABLED = "false"

[tools]
filesystem = true
custom_tools = []

[agent]
name = "Research Assistant"
system_prompt = """
You are a helpful research assistant with access to various tools and a knowledge base.
Always provide accurate information and cite your sources when available.
When asked about topics in the knowledge base, search for relevant information first.
"""
context = [
    "You have access to a vector store containing technical documentation.",
    "You can use tools to help answer questions.",
    "Be concise but thorough in your responses."
]
temperature = 0.7
"#;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Debug Config Parsing ===\n");

    // Load and parse the test config
    let config = load_config_from_str(TEST_CONFIG)?;

    // Print the parsed config structure
    println!("✅ Config parsed successfully!\n");
    println!("📋 Parsed configuration structure:");
    println!("{config:#?}");

    // Test specific parts
    println!("\n🔍 Specific sections:");
    let (provider, model) = match &config.llm {
        aura_config::config::LlmConfig::OpenAI { model, .. } => ("openai", model.as_str()),
        aura_config::config::LlmConfig::Anthropic { model, .. } => ("anthropic", model.as_str()),
        aura_config::config::LlmConfig::Bedrock { model, .. } => ("bedrock", model.as_str()),
        aura_config::config::LlmConfig::Gemini { model, .. } => ("gemini", model.as_str()),
        aura_config::config::LlmConfig::Ollama { model, .. } => ("ollama", model.as_str()),
    };
    println!("LLM Provider: {provider} ({model})");

    if let Some(mcp_config) = &config.mcp {
        println!("MCP Servers: {} configured", mcp_config.servers.len());
        for (name, server) in &mcp_config.servers {
            match server {
                aura_config::config::McpServerConfig::HttpStreamable {
                    url,
                    headers,
                    description,
                    headers_from_request,
                } => {
                    println!(
                        "  - {}: HTTP Streamable at {} ({} headers)",
                        name,
                        url,
                        headers.len()
                    );
                    if let Some(desc) = description {
                        println!("    Description: {desc}");
                    }
                    println!(
                        "    Headers from requests: {} to forward",
                        headers_from_request.len()
                    );
                }
                aura_config::config::McpServerConfig::Stdio {
                    cmd,
                    args,
                    env,
                    description,
                } => {
                    println!(
                        "  - {}: STDIO command {:?} with {} args, {} env vars",
                        name,
                        cmd,
                        args.len(),
                        env.len()
                    );
                    if let Some(desc) = description {
                        println!("    Description: {desc}");
                    }
                }
            }
        }
    } else {
        println!("No MCP servers configured");
    }

    if let Some(tools) = &config.tools {
        println!(
            "Tools: filesystem={}, custom_tools={:?}",
            tools.filesystem, tools.custom_tools
        );
    }

    println!(
        "Agent: {} (temp: {:?})",
        config.agent.name, config.agent.temperature
    );
    if !config.vector_stores.is_empty() {
        println!("Vector Stores: {} configured", config.vector_stores.len());
        for (i, store) in config.vector_stores.iter().enumerate() {
            println!("  Store {}: {} @ {}", i + 1, store.name, store.url);
            println!("    Collection: {}", store.collection_name);
            println!(
                "    Embedding Model: {} {}",
                store.embedding_model.provider, store.embedding_model.model
            );
        }
    } else {
        println!("Vector Stores: None configured");
    }

    Ok(())
}
