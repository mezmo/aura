//! Native Bedrock embedding model for aura.
//!
//! Bypasses `rig-bedrock` which hard-codes the Titan v2 request shape and
//! always serializes `dimensions: 0` when ndims is not set. This type
//! talks to `aws-sdk-bedrockruntime` directly and emits the correct body
//! per model family (Titan v1, Titan v2, Cohere embed v3).

use aws_sdk_bedrockruntime::Client as BedrockClient;
use aws_sdk_bedrockruntime::primitives::Blob;
use rig::embeddings::{Embedding, EmbeddingError, EmbeddingModel};
use serde::{Deserialize, Serialize};

#[derive(Clone)]
pub struct AuraBedrockEmbeddingModel {
    client: BedrockClient,
    model: String,
    ndims: usize,
}

impl AuraBedrockEmbeddingModel {
    pub fn new(client: BedrockClient, model: impl Into<String>, ndims: Option<usize>) -> Self {
        let model = model.into();
        let ndims = ndims.unwrap_or_else(|| default_ndims(&model));
        Self {
            client,
            model,
            ndims,
        }
    }
}

fn default_ndims(model: &str) -> usize {
    if model.starts_with("amazon.titan-embed-text-v2") {
        1024
    } else if model.starts_with("amazon.titan-embed-text-v1") {
        1536
    } else if model.starts_with("cohere.embed-") {
        1024
    } else {
        1024
    }
}

#[derive(Serialize)]
struct TitanV2Req<'a> {
    #[serde(rename = "inputText")]
    input_text: &'a str,
    dimensions: usize,
    normalize: bool,
}

#[derive(Serialize)]
struct TitanV1Req<'a> {
    #[serde(rename = "inputText")]
    input_text: &'a str,
}

#[derive(Serialize)]
struct CohereReq<'a> {
    texts: Vec<&'a str>,
    input_type: &'static str,
    truncate: &'static str,
}

#[derive(Deserialize)]
struct TitanResp {
    embedding: Vec<f64>,
}

#[derive(Deserialize)]
struct CohereResp {
    embeddings: Vec<Vec<f64>>,
}

impl AuraBedrockEmbeddingModel {
    async fn invoke_one(&self, text: &str) -> Result<Vec<f64>, EmbeddingError> {
        let body = if self.model.starts_with("amazon.titan-embed-text-v2") {
            serde_json::to_vec(&TitanV2Req {
                input_text: text,
                dimensions: self.ndims,
                normalize: true,
            })
        } else if self.model.starts_with("amazon.titan-embed-text-v1") {
            serde_json::to_vec(&TitanV1Req { input_text: text })
        } else if self.model.starts_with("cohere.embed-") {
            serde_json::to_vec(&CohereReq {
                texts: vec![text],
                input_type: "search_document",
                truncate: "END",
            })
        } else {
            return Err(EmbeddingError::ProviderError(format!(
                "unsupported Bedrock embedding model: {}",
                self.model
            )));
        }
        .map_err(EmbeddingError::JsonError)?;

        let resp = self
            .client
            .invoke_model()
            .model_id(&self.model)
            .content_type("application/json")
            .accept("application/json")
            .body(Blob::new(body))
            .send()
            .await
            .map_err(|e| EmbeddingError::ProviderError(format!("{e:?}")))?;

        let bytes = resp.body.into_inner();
        if self.model.starts_with("cohere.embed-") {
            let parsed: CohereResp =
                serde_json::from_slice(&bytes).map_err(EmbeddingError::JsonError)?;
            parsed
                .embeddings
                .into_iter()
                .next()
                .ok_or_else(|| EmbeddingError::ResponseError("empty cohere response".into()))
        } else {
            let parsed: TitanResp =
                serde_json::from_slice(&bytes).map_err(EmbeddingError::JsonError)?;
            Ok(parsed.embedding)
        }
    }
}

impl EmbeddingModel for AuraBedrockEmbeddingModel {
    const MAX_DOCUMENTS: usize = 1;
    type Client = BedrockClient;

    fn make(client: &Self::Client, model: impl Into<String>, dims: Option<usize>) -> Self {
        Self::new(client.clone(), model, dims)
    }

    fn ndims(&self) -> usize {
        self.ndims
    }

    async fn embed_texts(
        &self,
        texts: impl IntoIterator<Item = String> + Send,
    ) -> Result<Vec<Embedding>, EmbeddingError> {
        let docs: Vec<String> = texts.into_iter().collect();
        let mut out = Vec::with_capacity(docs.len());
        for doc in docs {
            let vec = self.invoke_one(&doc).await?;
            out.push(Embedding { document: doc, vec });
        }
        Ok(out)
    }
}
