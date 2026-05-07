# Vector Stores

Aura supports RAG (Retrieval-Augmented Generation) through vector stores for semantic search. Each configured vector store exposes a `vector_search_<name>` tool the agent can call to retrieve relevant context.

## Supported Vector Stores

| Type | Description | Embedding Model Required |
|------|-------------|-------------------------|
| `in_memory` | Ephemeral store for development and testing | Yes |
| `qdrant` | Self-hosted [Qdrant](https://qdrant.tech) instance | Yes |
| `bedrock_kb` | AWS Bedrock Knowledge Bases (managed RAG) | No (managed internally) |

## Supported Embedding Providers

`in_memory` and `qdrant` stores require an embedding model:

| Provider | Models | Authentication |
|----------|--------|----------------|
| `openai` | `text-embedding-3-small`, `text-embedding-3-large`, `text-embedding-ada-002` | API key |
| `bedrock` | `amazon.titan-embed-text-v2:0`, `amazon.titan-embed-text-v1`, `cohere.embed-*` | AWS credentials |

## Configuration Examples

### Qdrant with OpenAI Embeddings

```toml
[[vector_stores]]
name = "docs"
type = "qdrant"
url = "http://localhost:6334"
collection_name = "documents"
context_prefix = "Technical documentation and API references"

[vector_stores.embedding_model]
provider = "openai"
model = "text-embedding-3-small"
api_key = "{{ env.OPENAI_API_KEY }}"
```

### Qdrant with Bedrock Embeddings

```toml
[[vector_stores]]
name = "docs"
type = "qdrant"
url = "http://localhost:6334"
collection_name = "documents"
context_prefix = "Technical documentation"

[vector_stores.embedding_model]
provider = "bedrock"
model = "amazon.titan-embed-text-v2:0"
region = "{{ env.AWS_REGION }}"
# profile = "default"  # optional AWS profile
```

### AWS Bedrock Knowledge Base

Bedrock Knowledge Bases are fully managed—AWS handles document ingestion, chunking, and embeddings. No embedding model configuration is needed.

```toml
[[vector_stores]]
name = "company_docs"
type = "bedrock_kb"
knowledge_base_id = "{{ env.BEDROCK_KB_ID }}"
region = "{{ env.AWS_REGION }}"
# profile = "default"  # optional AWS profile
context_prefix = "Company documentation"
```

### In-Memory (Development)

```toml
[[vector_stores]]
name = "scratch"
type = "in_memory"

[vector_stores.embedding_model]
provider = "openai"
model = "text-embedding-3-small"
api_key = "{{ env.OPENAI_API_KEY }}"
```

## Multiple Vector Stores

You can configure multiple stores. Each becomes a separate search tool:

```toml
[[vector_stores]]
name = "docs"
type = "qdrant"
url = "http://localhost:6334"
collection_name = "documentation"

[vector_stores.embedding_model]
provider = "openai"
model = "text-embedding-3-small"
api_key = "{{ env.OPENAI_API_KEY }}"

[[vector_stores]]
name = "runbooks"
type = "bedrock_kb"
knowledge_base_id = "KB12345"
region = "us-west-2"
context_prefix = "Operational runbooks and troubleshooting guides"
```

The agent receives two tools: `vector_search_docs` and `vector_search_runbooks`.

## Configuration Reference

### Common Fields

| Field | Required | Description |
|-------|----------|-------------|
| `name` | Yes | Unique identifier; becomes part of the tool name |
| `type` | Yes | `in_memory`, `qdrant`, or `bedrock_kb` |
| `context_prefix` | No | Description of store contents for better LLM guidance |

### Qdrant-Specific Fields

| Field | Required | Description |
|-------|----------|-------------|
| `url` | Yes | Qdrant gRPC endpoint (typically port 6334) |
| `collection_name` | Yes | Name of the Qdrant collection to search |
| `embedding_model` | Yes | Embedding configuration (see below) |

### Bedrock KB-Specific Fields

| Field | Required | Description |
|-------|----------|-------------|
| `knowledge_base_id` | Yes | AWS Bedrock Knowledge Base ID |
| `region` | Yes | AWS region where the KB is deployed |
| `profile` | No | AWS credentials profile name |

### Embedding Model Configuration

**OpenAI:**

```toml
[vector_stores.embedding_model]
provider = "openai"
model = "text-embedding-3-small"
api_key = "{{ env.OPENAI_API_KEY }}"
```

**Bedrock:**

```toml
[vector_stores.embedding_model]
provider = "bedrock"
model = "amazon.titan-embed-text-v2:0"
region = "{{ env.AWS_REGION }}"
profile = "default"  # optional
```

## AWS Authentication

Bedrock embeddings and Knowledge Bases use AWS credentials from the environment. Authentication is resolved in order:

1. `profile` field in config (if specified)
2. `AWS_PROFILE` environment variable
3. Default credentials chain (environment variables, IAM role, etc.)

Ensure your credentials have permissions for:
- Bedrock embeddings: `bedrock:InvokeModel`
- Bedrock Knowledge Bases: `bedrock:Retrieve`

## Notes

- **Bedrock KB is read-only**: Document ingestion is managed through the AWS console or API, not through Aura.
- **Qdrant setup**: You must create and populate Qdrant collections before the agent can search them. See the [Qdrant documentation](https://qdrant.tech/documentation/concepts/collections/).
- **Embedding dimensions**: Aura automatically handles dimension configuration for supported Bedrock embedding models.
