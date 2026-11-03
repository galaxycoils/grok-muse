use crate::budget::BudgetManager;
use crate::router::ModelTarget;
use async_trait::async_trait;
use gm_core::error::{GmError, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{info, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

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

#[async_trait]
pub trait LlmBackend: Send + Sync {
    async fn complete(
        &self,
        target: &ModelTarget,
        messages: &[ChatMessage],
    ) -> Result<ChatCompletionResponse>;
}

pub struct MockLlmBackend {
    next_response: Arc<Mutex<Option<String>>>,
    fail_first_n_calls: Arc<Mutex<u32>>,
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
            fail_first_n_calls: Arc::new(Mutex::new(0)),
        }
    }

    pub fn set_response(&self, content: impl Into<String>) {
        *self.next_response.lock() = Some(content.into());
    }

    pub fn set_fail_calls(&self, count: u32) {
        *self.fail_first_n_calls.lock() = count;
    }
}

#[async_trait]
impl LlmBackend for MockLlmBackend {
    async fn complete(
        &self,
        target: &ModelTarget,
        _messages: &[ChatMessage],
    ) -> Result<ChatCompletionResponse> {
        let mut fail_count = self.fail_first_n_calls.lock();
        if *fail_count > 0 {
            *fail_count -= 1;
            return Err(GmError::Provider(format!(
                "Mock failure for provider {:?}",
                target.provider
            )));
        }

        let content = self.next_response.lock().clone().unwrap_or_else(|| {
            "{\"action\":\"done\",\"params\":{\"summary\":\"mock completed\"}}".into()
        });

        Ok(ChatCompletionResponse {
            id: "mock_resp_1".into(),
            content,
            model: target.model.clone(),
            prompt_tokens: 50,
            completion_tokens: 20,
            estimated_cost_usd: 0.001,
        })
    }
}

pub struct HybridLlmClient {
    backend: Arc<dyn LlmBackend>,
    budget: Arc<BudgetManager>,
}

impl HybridLlmClient {
    pub fn new(backend: Arc<dyn LlmBackend>, budget: Arc<BudgetManager>) -> Self {
        Self { backend, budget }
    }

    pub async fn execute_routed(
        &self,
        primary: &ModelTarget,
        fallback: Option<&ModelTarget>,
        messages: &[ChatMessage],
    ) -> Result<ChatCompletionResponse> {
        match self.backend.complete(primary, messages).await {
            Ok(resp) => {
                self.budget.record_spend(resp.estimated_cost_usd)?;
                Ok(resp)
            }
            Err(e) => {
                warn!("Primary provider {:?} failed: {e}", primary.provider);
                if let Some(fallback_target) = fallback {
                    info!(
                        "Failing over to fallback provider {:?}",
                        fallback_target.provider
                    );
                    let resp = self.backend.complete(fallback_target, messages).await?;
                    self.budget.record_spend(resp.estimated_cost_usd)?;
                    Ok(resp)
                } else {
                    Err(e)
                }
            }
        }
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

        let msgs = vec![ChatMessage {
            role: "user".into(),
            content: "Click the Submit button".into(),
        }];

        let result = client
            .execute_routed(&primary, fallback.as_ref(), &msgs)
            .await;
        assert!(result.is_ok());
        let resp = result.unwrap();
        assert_eq!(resp.model, "grok-4"); // fallback succeeded!
    }
}
