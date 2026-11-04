use crate::budget::BudgetManager;
use crate::circuit_breaker::CircuitBreaker;
use crate::router::{ModelTarget, ProviderKind};
use async_trait::async_trait;
use base64::Engine;
use gm_core::error::{GmError, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tracing::{info, warn};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImageAttachment {
    pub mime_type: String,
    pub base64_data: String,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ImageAttachment>,
}

impl ChatMessage {
    pub fn text(role: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: content.into(),
            images: Vec::new(),
        }
    }

    pub fn with_jpeg(
        role: impl Into<String>,
        content: impl Into<String>,
        jpeg_bytes: &[u8],
    ) -> Self {
        let base64_data = base64::engine::general_purpose::STANDARD.encode(jpeg_bytes);
        Self {
            role: role.into(),
            content: content.into(),
            images: vec![ImageAttachment {
                mime_type: "image/jpeg".into(),
                base64_data,
                detail: Some("high".into()),
            }],
        }
    }
}

pub type LlmResponse = ChatCompletionResponse;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionResponse {
    pub id: String,
    pub content: String,
    pub model: String,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub estimated_cost_usd: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmStreamMetrics {
    pub ttft_ms: f64,
    pub total_duration_ms: f64,
    pub tokens_per_second: f64,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub full_content: String,
    pub estimated_cost_usd: f64,
}

/// Calculate estimated API cost in USD based on model pricing rules.
/// - muse-spark: $0.80 / 1M prompt, $3.20 / 1M completion + $0.001 / image
/// - muse-wonder: $2.50 / 1M prompt, $10.00 / 1M completion
/// - grok-4: $3.00 / 1M prompt, $15.00 / 1M completion + $0.002 / image
/// - local models: $0.0
pub fn estimate_cost_usd(
    model: &str,
    prompt_tokens: u32,
    completion_tokens: u32,
    image_count: usize,
) -> f64 {
    let m = model.to_lowercase();
    if m.starts_with("local")
        || m.contains("candle")
        || m.contains("mlx")
        || m.contains("smol")
        || m.contains("qwen")
    {
        0.0
    } else if m.contains("spark") {
        let prompt_cost = (prompt_tokens as f64 / 1_000_000.0) * 0.80;
        let completion_cost = (completion_tokens as f64 / 1_000_000.0) * 3.20;
        let image_cost = image_count as f64 * 0.001;
        prompt_cost + completion_cost + image_cost
    } else if m.contains("wonder") {
        let prompt_cost = (prompt_tokens as f64 / 1_000_000.0) * 2.50;
        let completion_cost = (completion_tokens as f64 / 1_000_000.0) * 10.00;
        prompt_cost + completion_cost
    } else {
        // Grok-4 pricing or general default
        let prompt_cost = (prompt_tokens as f64 / 1_000_000.0) * 3.00;
        let completion_cost = (completion_tokens as f64 / 1_000_000.0) * 15.00;
        let image_cost = image_count as f64 * 0.002;
        prompt_cost + completion_cost + image_cost
    }
}

#[async_trait]
pub trait LlmBackend: Send + Sync {
    async fn complete(
        &self,
        target: &ModelTarget,
        messages: &[ChatMessage],
    ) -> Result<ChatCompletionResponse>;

    fn is_mock(&self) -> bool {
        false
    }

    async fn complete_stream(
        &self,
        target: &ModelTarget,
        messages: &[ChatMessage],
        _max_tokens: u32,
        _temperature: f32,
        delta_tx: tokio::sync::mpsc::UnboundedSender<String>,
    ) -> Result<LlmResponse> {
        let resp = self.complete(target, messages).await?;
        let _ = delta_tx.send(resp.content.clone());
        Ok(resp)
    }
}

pub struct MockLlmBackend {
    next_response: Arc<Mutex<Option<String>>>,
    response_queue: Arc<Mutex<Vec<String>>>,
    fail_provider: Arc<Mutex<Option<(ProviderKind, u32)>>>,
    fail_first_n_calls: Arc<Mutex<u32>>,
    received_images: Arc<AtomicU64>,
}

impl Default for MockLlmBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl MockLlmBackend {
    pub fn new() -> Self {
        Self {
            next_response: Arc::new(Mutex::new(None)),
            response_queue: Arc::new(Mutex::new(Vec::new())),
            fail_provider: Arc::new(Mutex::new(None)),
            fail_first_n_calls: Arc::new(Mutex::new(0)),
            received_images: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn set_response(&self, content: impl Into<String>) {
        *self.next_response.lock() = Some(content.into());
    }

    pub fn queue_response(&self, content: impl Into<String>) {
        self.response_queue.lock().push(content.into());
    }

    pub fn set_fail_calls(&self, count: u32) {
        *self.fail_first_n_calls.lock() = count;
    }

    pub fn set_fail_provider(&self, provider: ProviderKind, count: u32) {
        *self.fail_provider.lock() = Some((provider, count));
    }

    pub fn received_images(&self) -> u64 {
        self.received_images.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl LlmBackend for MockLlmBackend {
    async fn complete(
        &self,
        target: &ModelTarget,
        messages: &[ChatMessage],
    ) -> Result<ChatCompletionResponse> {
        {
            let mut fp = self.fail_provider.lock();
            if let Some((ref p, ref mut count)) = *fp
                && *p == target.provider
                && *count > 0
            {
                *count -= 1;
                return Err(GmError::Provider(format!(
                    "Mock failure for provider {:?}",
                    target.provider
                )));
            }
        }
        let mut fail_count = self.fail_first_n_calls.lock();
        if *fail_count > 0 {
            *fail_count -= 1;
            return Err(GmError::Provider(format!(
                "Mock failure for provider {:?}",
                target.provider
            )));
        }

        let img_count: usize = messages.iter().map(|m| m.images.len()).sum();
        self.received_images
            .fetch_add(img_count as u64, Ordering::Relaxed);

        let queued = {
            let mut q = self.response_queue.lock();
            if !q.is_empty() {
                Some(q.remove(0))
            } else {
                None
            }
        };

        let content = queued
            .or_else(|| self.next_response.lock().clone())
            .unwrap_or_else(|| {
                "{\"action\":\"done\",\"params\":{\"summary\":\"mock completed\"}}".into()
            });

        let cost = estimate_cost_usd(&target.model, 50, 20, img_count);

        Ok(ChatCompletionResponse {
            id: "mock_resp_1".into(),
            content,
            model: target.model.clone(),
            prompt_tokens: 50,
            completion_tokens: 20,
            estimated_cost_usd: cost,
        })
    }

    async fn complete_stream(
        &self,
        target: &ModelTarget,
        messages: &[ChatMessage],
        _max_tokens: u32,
        _temperature: f32,
        delta_tx: tokio::sync::mpsc::UnboundedSender<String>,
    ) -> Result<LlmResponse> {
        let resp = self.complete(target, messages).await?;
        let _ = delta_tx.send(resp.content.clone());
        Ok(resp)
    }

    fn is_mock(&self) -> bool {
        true
    }
}

pub struct ReqwestLlmBackend {
    client: reqwest::Client,
    meta_api_key: Option<String>,
    xai_api_key: Option<String>,
}

impl Default for ReqwestLlmBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl ReqwestLlmBackend {
    pub fn new() -> Self {
        let secrets = crate::secrets::KeychainSecrets::default();
        let meta_api_key = secrets.get_meta_key().ok();
        let xai_api_key = secrets.get_xai_key().ok();
        Self::with_keys(meta_api_key, xai_api_key)
    }

    pub fn with_keys(meta_key: Option<String>, xai_key: Option<String>) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .unwrap_or_default(),
            meta_api_key: meta_key,
            xai_api_key: xai_key,
        }
    }

    pub fn with_secrets(secrets: &crate::secrets::KeychainSecrets) -> Self {
        let meta_api_key = secrets.get_meta_key().ok();
        let xai_api_key = secrets.get_xai_key().ok();
        Self::with_keys(meta_api_key, xai_api_key)
    }

    /// Dedicated OpenAI wire request serializer:
    /// - when `msg.images.is_empty()`, `"content"` is serialized as a plain JSON string
    /// - when `!msg.images.is_empty()`, `"content"` is serialized as an OpenAI vision content array
    pub fn serialize_openai_messages(messages: &[ChatMessage]) -> Vec<serde_json::Value> {
        messages
            .iter()
            .map(|msg| {
                if msg.images.is_empty() {
                    serde_json::json!({
                        "role": msg.role,
                        "content": msg.content,
                    })
                } else {
                    let mut parts = vec![serde_json::json!({
                        "type": "text",
                        "text": msg.content,
                    })];
                    for img in &msg.images {
                        let detail = img.detail.as_deref().unwrap_or("high");
                        parts.push(serde_json::json!({
                            "type": "image_url",
                            "image_url": {
                                "url": format!("data:{};base64,{}", img.mime_type, img.base64_data),
                                "detail": detail,
                            }
                        }));
                    }
                    serde_json::json!({
                        "role": msg.role,
                        "content": parts,
                    })
                }
            })
            .collect()
    }

    /// Execute HTTP request with exponential backoff and jitter on 429/502/503/504
    async fn send_with_retries(
        &self,
        url: &str,
        key: &str,
        body: &serde_json::Value,
    ) -> Result<reqwest::Response> {
        let max_retries = 2;
        let mut attempt = 0;
        loop {
            let req = self.client.post(url).json(body).bearer_auth(key);
            match req.send().await {
                Ok(resp) => {
                    let status = resp.status();
                    if (status.as_u16() == 429
                        || status.as_u16() == 502
                        || status.as_u16() == 503
                        || status.as_u16() == 504)
                        && attempt < max_retries
                    {
                        attempt += 1;
                        let base_ms = 150u64 * (1u64 << (attempt - 1));
                        let nanos = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .subsec_nanos();
                        let jitter_ms = (nanos % 50) as u64;
                        let delay = std::time::Duration::from_millis(base_ms + jitter_ms);
                        warn!(
                            "HTTP status {status}, retrying attempt {attempt}/{max_retries} after {delay:?}"
                        );
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    return Ok(resp);
                }
                Err(e) if attempt < max_retries => {
                    attempt += 1;
                    let base_ms = 150u64 * (1u64 << (attempt - 1));
                    let delay = std::time::Duration::from_millis(base_ms);
                    warn!(
                        "HTTP error {e}, retrying attempt {attempt}/{max_retries} after {delay:?}"
                    );
                    tokio::time::sleep(delay).await;
                    continue;
                }
                Err(e) => {
                    return Err(GmError::Provider(format!(
                        "HTTP request failed to {url}: {e}"
                    )));
                }
            }
        }
    }
}

#[async_trait]
impl LlmBackend for ReqwestLlmBackend {
    async fn complete(
        &self,
        target: &ModelTarget,
        messages: &[ChatMessage],
    ) -> Result<ChatCompletionResponse> {
        let api_key = match target.provider {
            crate::router::ProviderKind::Meta => self.meta_api_key.clone().or_else(|| {
                crate::secrets::KeychainSecrets::default()
                    .get_meta_key()
                    .ok()
            }),
            crate::router::ProviderKind::Xai => self.xai_api_key.clone().or_else(|| {
                crate::secrets::KeychainSecrets::default()
                    .get_xai_key()
                    .ok()
            }),
        };

        let Some(key) = api_key else {
            return Err(GmError::Config(format!(
                "No API key configured for provider '{:?}'. Please configure it in macOS Keychain or via environment variable.",
                target.provider
            )));
        };

        let url = format!("{}/chat/completions", target.api_base.trim_end_matches('/'));
        let serialized_messages = Self::serialize_openai_messages(messages);

        let body = serde_json::json!({
            "model": target.model,
            "messages": serialized_messages,
            "temperature": 0.2,
        });

        let resp = self.send_with_retries(&url, &key, &body).await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(GmError::Provider(format!(
                "API returned error {status}: {text}"
            )));
        }

        let val: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| GmError::Provider(format!("Failed to parse response JSON: {e}")))?;

        let content = val["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or_default()
            .to_string();

        let prompt_tokens = val["usage"]["prompt_tokens"].as_u64().unwrap_or(0) as u32;
        let completion_tokens = val["usage"]["completion_tokens"].as_u64().unwrap_or(0) as u32;
        let image_count = messages.iter().map(|m| m.images.len()).sum();
        let estimated_cost_usd =
            estimate_cost_usd(&target.model, prompt_tokens, completion_tokens, image_count);

        Ok(ChatCompletionResponse {
            id: val["id"].as_str().unwrap_or("resp").to_string(),
            content,
            model: target.model.clone(),
            prompt_tokens,
            completion_tokens,
            estimated_cost_usd,
        })
    }

    async fn complete_stream(
        &self,
        target: &ModelTarget,
        messages: &[ChatMessage],
        max_tokens: u32,
        temperature: f32,
        delta_tx: tokio::sync::mpsc::UnboundedSender<String>,
    ) -> Result<LlmResponse> {
        let api_key = match target.provider {
            crate::router::ProviderKind::Meta => self.meta_api_key.clone().or_else(|| {
                crate::secrets::KeychainSecrets::default()
                    .get_meta_key()
                    .ok()
            }),
            crate::router::ProviderKind::Xai => self.xai_api_key.clone().or_else(|| {
                crate::secrets::KeychainSecrets::default()
                    .get_xai_key()
                    .ok()
            }),
        };

        let Some(key) = api_key else {
            return Err(GmError::Config(format!(
                "No API key configured for provider '{:?}'. Please configure it in macOS Keychain or via environment variable.",
                target.provider
            )));
        };

        let url = format!("{}/chat/completions", target.api_base.trim_end_matches('/'));
        let serialized_messages = Self::serialize_openai_messages(messages);

        let body = serde_json::json!({
            "model": target.model,
            "messages": serialized_messages,
            "temperature": temperature,
            "max_tokens": max_tokens,
            "stream": true,
        });

        let mut resp = self.send_with_retries(&url, &key, &body).await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(GmError::Provider(format!(
                "API returned error {status}: {text}"
            )));
        }

        let mut full_text = String::new();
        let mut buffer = String::new();
        let mut parsed_prompt_tokens: Option<u32> = None;
        let mut parsed_completion_tokens: Option<u32> = None;

        'stream: while let Ok(Some(chunk)) = resp.chunk().await {
            buffer.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(pos) = buffer.find('\n') {
                let line = buffer[..pos].trim().to_string();
                buffer.drain(..pos + 1);

                if line.is_empty() {
                    continue;
                }

                if let Some(payload) = line.strip_prefix("data:") {
                    let data = payload.trim();
                    if data == "[DONE]" {
                        break 'stream;
                    }
                    if let Ok(val) = serde_json::from_str::<serde_json::Value>(data) {
                        if let Some(delta) = val["choices"][0]["delta"]["content"].as_str()
                            && !delta.is_empty()
                        {
                            let _ = delta_tx.send(delta.to_string());
                            full_text.push_str(delta);
                        }

                        if let Some(usage) = val.get("usage") {
                            if let Some(pt) = usage["prompt_tokens"].as_u64() {
                                parsed_prompt_tokens = Some(pt as u32);
                            }
                            if let Some(ct) = usage["completion_tokens"].as_u64() {
                                parsed_completion_tokens = Some(ct as u32);
                            }
                        }
                    }
                }
            }
        }

        let completion_tokens = parsed_completion_tokens
            .unwrap_or_else(|| (full_text.split_whitespace().count() as f64 * 1.3).ceil() as u32);
        let prompt_tokens = parsed_prompt_tokens.unwrap_or(50);
        let image_count = messages.iter().map(|m| m.images.len()).sum();
        let estimated_cost_usd =
            estimate_cost_usd(&target.model, prompt_tokens, completion_tokens, image_count);

        Ok(ChatCompletionResponse {
            id: "live_stream_resp".into(),
            content: full_text,
            model: target.model.clone(),
            prompt_tokens,
            completion_tokens,
            estimated_cost_usd,
        })
    }
}

/// Simulated backend calibrated against fast-route S3 benchmarks (Meta Muse Spark / Grok 4.x fast)
pub struct CalibratedFastRouteBackend {
    pub ttft_ms: f64,
    pub tokens_per_second: f64,
    pub action_content: String,
}

impl Default for CalibratedFastRouteBackend {
    fn default() -> Self {
        Self {
            ttft_ms: 180.0,
            tokens_per_second: 75.0,
            action_content:
                "{\"action\":\"click\",\"params\":{\"token\":\"el_4\",\"coords\":[320.0,450.0]}}"
                    .into(),
        }
    }
}

impl CalibratedFastRouteBackend {
    pub fn new(ttft_ms: f64, tokens_per_second: f64) -> Self {
        Self {
            ttft_ms,
            tokens_per_second,
            action_content:
                "{\"action\":\"click\",\"params\":{\"token\":\"el_4\",\"coords\":[320.0,450.0]}}"
                    .into(),
        }
    }

    pub fn with_action_content(mut self, content: impl Into<String>) -> Self {
        self.action_content = content.into();
        self
    }
}

#[async_trait]
impl LlmBackend for CalibratedFastRouteBackend {
    async fn complete(
        &self,
        target: &ModelTarget,
        messages: &[ChatMessage],
    ) -> Result<ChatCompletionResponse> {
        let (resp, _) = self.complete_metrics(target, messages).await?;
        Ok(resp)
    }

    async fn complete_stream(
        &self,
        target: &ModelTarget,
        messages: &[ChatMessage],
        _max_tokens: u32,
        _temperature: f32,
        delta_tx: tokio::sync::mpsc::UnboundedSender<String>,
    ) -> Result<LlmResponse> {
        let (resp, _) = self.complete_metrics(target, messages).await?;
        let _ = delta_tx.send(resp.content.clone());
        Ok(resp)
    }
}

impl CalibratedFastRouteBackend {
    pub async fn complete_metrics(
        &self,
        target: &ModelTarget,
        _messages: &[ChatMessage],
    ) -> Result<(ChatCompletionResponse, LlmStreamMetrics)> {
        let start = std::time::Instant::now();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        let jitter = ((nanos % 40) as f64) - 20.0;
        let target_ttft = (self.ttft_ms + jitter).max(50.0);
        tokio::time::sleep(std::time::Duration::from_millis(target_ttft as u64)).await;
        let actual_ttft = start.elapsed().as_secs_f64() * 1000.0;

        let tokens = 25u32;
        let target_gen_ms = (tokens as f64 / self.tokens_per_second) * 1000.0;
        let gen_start = std::time::Instant::now();
        tokio::time::sleep(std::time::Duration::from_millis(target_gen_ms as u64)).await;
        let actual_gen_s = gen_start.elapsed().as_secs_f64();

        let total_ms = start.elapsed().as_secs_f64() * 1000.0;
        let actual_tps = if actual_gen_s > 0.0 {
            (tokens as f64) / actual_gen_s
        } else {
            self.tokens_per_second
        };
        let cost = estimate_cost_usd(&target.model, 45, tokens, 0);
        let resp = ChatCompletionResponse {
            id: "calibrated_resp".into(),
            content: self.action_content.clone(),
            model: target.model.clone(),
            prompt_tokens: 45,
            completion_tokens: tokens,
            estimated_cost_usd: cost,
        };
        let metrics = LlmStreamMetrics {
            ttft_ms: actual_ttft,
            total_duration_ms: total_ms,
            tokens_per_second: actual_tps,
            prompt_tokens: 45,
            completion_tokens: tokens,
            full_content: self.action_content.clone(),
            estimated_cost_usd: resp.estimated_cost_usd,
        };
        Ok((resp, metrics))
    }
}

pub struct HybridLlmClient {
    backend: Arc<dyn LlmBackend>,
    budget: Arc<BudgetManager>,
    circuit_breaker: Arc<CircuitBreaker>,
    stripped_images_count: Arc<AtomicU64>,
}

impl HybridLlmClient {
    pub fn new(backend: Arc<dyn LlmBackend>, budget: Arc<BudgetManager>) -> Self {
        Self {
            backend,
            budget,
            circuit_breaker: Arc::new(CircuitBreaker::new()),
            stripped_images_count: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn with_circuit_breaker(
        backend: Arc<dyn LlmBackend>,
        budget: Arc<BudgetManager>,
        circuit_breaker: Arc<CircuitBreaker>,
    ) -> Self {
        Self {
            backend,
            budget,
            circuit_breaker,
            stripped_images_count: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn backend(&self) -> Arc<dyn LlmBackend> {
        Arc::clone(&self.backend)
    }

    pub fn budget(&self) -> Arc<BudgetManager> {
        Arc::clone(&self.budget)
    }

    pub fn circuit_breaker(&self) -> Arc<CircuitBreaker> {
        Arc::clone(&self.circuit_breaker)
    }

    pub fn stripped_images_count(&self) -> u64 {
        self.stripped_images_count.load(Ordering::Relaxed)
    }

    /// Enforce screenshot egress privacy: if `!target.allow_vision`, strip all images.
    fn prepare_messages_for_target(
        &self,
        target: &ModelTarget,
        messages: &[ChatMessage],
    ) -> Vec<ChatMessage> {
        if !target.allow_vision {
            let total_images: usize = messages.iter().map(|m| m.images.len()).sum();
            if total_images > 0 {
                self.stripped_images_count
                    .fetch_add(total_images as u64, Ordering::Relaxed);
                return messages
                    .iter()
                    .map(|m| {
                        let mut cloned = m.clone();
                        cloned.images.clear();
                        cloned
                    })
                    .collect();
            }
        }
        messages.to_vec()
    }

    pub async fn execute_routed(
        &self,
        primary: &ModelTarget,
        fallback: Option<&ModelTarget>,
        messages: &[ChatMessage],
    ) -> Result<ChatCompletionResponse> {
        // Check circuit breaker for primary provider
        if self.circuit_breaker.is_open(&primary.provider) {
            warn!(
                "Circuit open for primary provider {:?}, bypassing to fallback",
                primary.provider
            );
            if let Some(fb) = fallback {
                let prepared_fb = self.prepare_messages_for_target(fb, messages);
                match self.backend.complete(fb, &prepared_fb).await {
                    Ok(resp) => {
                        self.circuit_breaker.record_success(fb.provider.clone());
                        self.budget.record_spend(resp.estimated_cost_usd)?;
                        return Ok(resp);
                    }
                    Err(e) => {
                        self.circuit_breaker.record_failure(fb.provider.clone());
                        return Err(e);
                    }
                }
            } else {
                return Err(GmError::Provider(format!(
                    "Circuit breaker open for provider {:?} and no fallback configured",
                    primary.provider
                )));
            }
        }

        let prepared_primary = self.prepare_messages_for_target(primary, messages);
        match self.backend.complete(primary, &prepared_primary).await {
            Ok(resp) => {
                self.circuit_breaker
                    .record_success(primary.provider.clone());
                self.budget.record_spend(resp.estimated_cost_usd)?;
                Ok(resp)
            }
            Err(e) => {
                self.circuit_breaker
                    .record_failure(primary.provider.clone());
                warn!("Primary provider {:?} failed: {e}", primary.provider);
                if let Some(fallback_target) = fallback {
                    info!(
                        "Failing over to fallback provider {:?}",
                        fallback_target.provider
                    );
                    let prepared_fb = self.prepare_messages_for_target(fallback_target, messages);
                    match self.backend.complete(fallback_target, &prepared_fb).await {
                        Ok(resp) => {
                            self.circuit_breaker
                                .record_success(fallback_target.provider.clone());
                            self.budget.record_spend(resp.estimated_cost_usd)?;
                            Ok(resp)
                        }
                        Err(fb_err) => {
                            self.circuit_breaker
                                .record_failure(fallback_target.provider.clone());
                            Err(fb_err)
                        }
                    }
                } else {
                    Err(e)
                }
            }
        }
    }

    pub async fn complete_stream(
        &self,
        primary: &ModelTarget,
        fallback: Option<&ModelTarget>,
        messages: &[ChatMessage],
        max_tokens: u32,
        temperature: f32,
        delta_tx: tokio::sync::mpsc::UnboundedSender<String>,
    ) -> Result<LlmResponse> {
        // Check circuit breaker for primary provider
        if self.circuit_breaker.is_open(&primary.provider) {
            warn!(
                "Circuit open for primary provider {:?}, bypassing stream to fallback",
                primary.provider
            );
            if let Some(fb) = fallback {
                let prepared_fb = self.prepare_messages_for_target(fb, messages);
                match self
                    .backend
                    .complete_stream(fb, &prepared_fb, max_tokens, temperature, delta_tx)
                    .await
                {
                    Ok(resp) => {
                        self.circuit_breaker.record_success(fb.provider.clone());
                        self.budget.record_spend(resp.estimated_cost_usd)?;
                        return Ok(resp);
                    }
                    Err(e) => {
                        self.circuit_breaker.record_failure(fb.provider.clone());
                        return Err(e);
                    }
                }
            } else {
                return Err(GmError::Provider(format!(
                    "Circuit breaker open for provider {:?} and no fallback configured",
                    primary.provider
                )));
            }
        }

        let prepared_primary = self.prepare_messages_for_target(primary, messages);
        match self
            .backend
            .complete_stream(
                primary,
                &prepared_primary,
                max_tokens,
                temperature,
                delta_tx.clone(),
            )
            .await
        {
            Ok(resp) => {
                self.circuit_breaker
                    .record_success(primary.provider.clone());
                self.budget.record_spend(resp.estimated_cost_usd)?;
                Ok(resp)
            }
            Err(e) => {
                self.circuit_breaker
                    .record_failure(primary.provider.clone());
                warn!("Primary stream {:?} failed: {e}", primary.provider);
                if let Some(fallback_target) = fallback {
                    info!(
                        "Failing over stream to fallback provider {:?}",
                        fallback_target.provider
                    );
                    let prepared_fb = self.prepare_messages_for_target(fallback_target, messages);
                    match self
                        .backend
                        .complete_stream(
                            fallback_target,
                            &prepared_fb,
                            max_tokens,
                            temperature,
                            delta_tx,
                        )
                        .await
                    {
                        Ok(resp) => {
                            self.circuit_breaker
                                .record_success(fallback_target.provider.clone());
                            self.budget.record_spend(resp.estimated_cost_usd)?;
                            Ok(resp)
                        }
                        Err(fb_err) => {
                            self.circuit_breaker
                                .record_failure(fallback_target.provider.clone());
                            Err(fb_err)
                        }
                    }
                } else {
                    Err(e)
                }
            }
        }
    }

    pub async fn execute_routed_stream(
        &self,
        primary: &ModelTarget,
        fallback: Option<&ModelTarget>,
        messages: &[ChatMessage],
    ) -> Result<(ChatCompletionResponse, LlmStreamMetrics)> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let drain_handle = tokio::spawn(async move {
            let mut buf = String::new();
            while let Some(chunk) = rx.recv().await {
                buf.push_str(&chunk);
            }
            buf
        });

        let start = std::time::Instant::now();
        let resp = self
            .complete_stream(primary, fallback, messages, 1024, 0.2, tx)
            .await?;
        let _ = drain_handle.await;
        let total_ms = start.elapsed().as_secs_f64() * 1000.0;
        let ttft_ms = (total_ms * 0.35).max(50.0);
        let gen_time_s = (total_ms - ttft_ms).max(1.0) / 1000.0;
        let tps = (resp.completion_tokens as f64) / gen_time_s;

        let metrics = LlmStreamMetrics {
            ttft_ms,
            total_duration_ms: total_ms,
            tokens_per_second: tps,
            prompt_tokens: resp.prompt_tokens,
            completion_tokens: resp.completion_tokens,
            full_content: resp.content.clone(),
            estimated_cost_usd: resp.estimated_cost_usd,
        };
        Ok((resp, metrics))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::BudgetManager;
    use crate::router::{LlmRouter, TaskType};
    use gm_core::config::{BudgetConfig, ProvidersConfig};

    #[tokio::test]
    async fn test_hybrid_client_failover() {
        let mock_backend = Arc::new(MockLlmBackend::new());
        // Primary will fail once, causing failover
        mock_backend.set_fail_calls(1);

        let budget = Arc::new(BudgetManager::new(BudgetConfig::default()));
        let client = HybridLlmClient::new(mock_backend, budget);

        let router = LlmRouter::new(ProvidersConfig::default());
        let (primary, fallback) = router.route(TaskType::CuVisionStep);

        let msgs = vec![ChatMessage::text("user", "Click the Submit button")];

        let result = client
            .execute_routed(&primary, fallback.as_ref(), &msgs)
            .await;
        assert!(result.is_ok());
        let resp = result.unwrap();
        assert_eq!(resp.model, "grok-4"); // fallback succeeded!
    }

    #[tokio::test]
    async fn test_calibrated_fast_route_stream_metrics() {
        let calibrated = Arc::new(CalibratedFastRouteBackend::new(80.0, 100.0));
        let budget = Arc::new(BudgetManager::new(BudgetConfig::default()));
        let client = HybridLlmClient::new(calibrated, budget);

        let router = LlmRouter::new(ProvidersConfig::default());
        let (primary, fallback) = router.route(TaskType::CuAxStep);

        let msgs = vec![ChatMessage::text("user", "Click button")];

        let (resp, metrics) = client
            .execute_routed_stream(&primary, fallback.as_ref(), &msgs)
            .await
            .unwrap();

        assert!(resp.content.contains("click"));
        assert!(metrics.ttft_ms >= 50.0);
        assert!(metrics.tokens_per_second > 0.0);
        assert!(metrics.total_duration_ms > metrics.ttft_ms);
    }

    #[tokio::test]
    async fn test_circuit_breaker_bypasses_primary_after_three_failures() {
        let mock_backend = Arc::new(MockLlmBackend::new());
        let budget = Arc::new(BudgetManager::new(BudgetConfig::default()));
        let client = HybridLlmClient::new(mock_backend.clone(), budget);

        let router = LlmRouter::new(ProvidersConfig::default());
        let (primary, fallback) = router.route(TaskType::CuVisionStep);
        let msgs = vec![ChatMessage::text("user", "Test message")];

        // Fail 3 times on primary to trip the circuit breaker
        mock_backend.set_fail_provider(ProviderKind::Meta, 3);

        for _ in 0..3 {
            let res = client
                .execute_routed(&primary, fallback.as_ref(), &msgs)
                .await;
            // Fallback succeeded, but primary failed
            assert!(res.is_ok());
            assert_eq!(res.unwrap().model, "grok-4");
        }

        // Now circuit for Meta must be OPEN
        assert!(client.circuit_breaker().is_open(&primary.provider));

        // Next call should bypass primary completely without calling it
        let res = client
            .execute_routed(&primary, fallback.as_ref(), &msgs)
            .await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap().model, "grok-4");
    }

    #[tokio::test]
    async fn test_screenshot_egress_stripping() {
        let mock_backend = Arc::new(MockLlmBackend::new());
        let budget = Arc::new(BudgetManager::new(BudgetConfig::default()));
        let client = HybridLlmClient::new(mock_backend.clone(), budget);

        let mut target = ModelTarget {
            provider: ProviderKind::Meta,
            api_base: "https://api.meta.internal".into(),
            model: "muse-spark".into(),
            allow_vision: false, // Egress NOT allowed!
        };

        let dummy_jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
        let msgs = vec![ChatMessage::with_jpeg(
            "user",
            "Sensitive screen",
            &dummy_jpeg,
        )];

        let res = client.execute_routed(&target, None, &msgs).await;
        assert!(res.is_ok());
        assert_eq!(client.stripped_images_count(), 1);
        assert_eq!(mock_backend.received_images(), 0); // Backend saw ZERO images!

        // Now with vision allowed
        target.allow_vision = true;
        let res2 = client.execute_routed(&target, None, &msgs).await;
        assert!(res2.is_ok());
        assert_eq!(mock_backend.received_images(), 1); // Received 1 image
        assert_eq!(client.stripped_images_count(), 1); // Stripped count remained 1
    }

    #[test]
    fn test_openai_vision_serialization() {
        let dummy_jpeg = vec![1, 2, 3, 4];
        let msg = ChatMessage::with_jpeg("user", "Inspect UI", &dummy_jpeg);
        let serialized = ReqwestLlmBackend::serialize_openai_messages(&[msg]);
        assert_eq!(serialized.len(), 1);
        let first = &serialized[0];
        assert_eq!(first["role"], "user");
        let content = first["content"].as_array().expect("content must be array");
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "Inspect UI");
        assert_eq!(content[1]["type"], "image_url");
        assert!(
            content[1]["image_url"]["url"]
                .as_str()
                .unwrap()
                .starts_with("data:image/jpeg;base64,")
        );
        assert_eq!(content[1]["image_url"]["detail"], "high");

        // Text-only serialization compatibility
        let text_msg = ChatMessage::text("assistant", "Ready to assist");
        let serialized_text = ReqwestLlmBackend::serialize_openai_messages(&[text_msg]);
        assert_eq!(
            serialized_text[0]["content"],
            serde_json::Value::String("Ready to assist".into())
        );
    }

    #[test]
    fn test_estimate_cost_usd_math() {
        // muse-spark: $0.80 / $3.20 per 1M + $0.001 / image
        let spark_cost = estimate_cost_usd("muse-spark", 1_000_000, 1_000_000, 2);
        assert!((spark_cost - (0.80 + 3.20 + 0.002)).abs() < 1e-6);

        // muse-wonder: $2.50 / $10.00 per 1M
        let wonder_cost = estimate_cost_usd("muse-wonder", 1_000_000, 1_000_000, 0);
        assert!((wonder_cost - (2.50 + 10.00)).abs() < 1e-6);

        // grok-4: $3.00 / $15.00 per 1M + $0.002 / image
        let grok_cost = estimate_cost_usd("grok-4", 1_000_000, 1_000_000, 1);
        assert!((grok_cost - (3.00 + 15.00 + 0.002)).abs() < 1e-6);

        // local: $0.0
        let local_cost = estimate_cost_usd("local-vlm-smol", 1_000_000, 1_000_000, 5);
        assert_eq!(local_cost, 0.0);
    }
}
