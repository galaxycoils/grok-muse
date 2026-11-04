use gm_core::config::ProvidersConfig;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TaskType {
    CuVisionStep,
    CuAxStep,
    Planning,
    ProactiveTriage,
    LiveWebSearch,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ProviderKind {
    Meta,
    Xai,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelTarget {
    pub provider: ProviderKind,
    pub api_base: String,
    pub model: String,
    pub allow_vision: bool,
}

#[derive(Debug, Clone)]
pub struct LlmRouter {
    config: ProvidersConfig,
    preference: Option<(ProviderKind, Option<String>)>,
}

impl LlmRouter {
    pub fn new(config: ProvidersConfig) -> Self {
        Self {
            config,
            preference: None,
        }
    }

    pub fn with_preference(
        mut self,
        provider: Option<&str>,
        model: Option<&str>,
    ) -> gm_core::error::Result<Self> {
        self.preference = match provider {
            Some("meta" | "muse") => Some((ProviderKind::Meta, model.map(str::to_string))),
            Some("xai" | "grok") => Some((ProviderKind::Xai, model.map(str::to_string))),
            Some(other) => {
                return Err(gm_core::error::GmError::Config(format!(
                    "Unsupported bot provider: {other}"
                )));
            }
            None if model.is_some() => {
                return Err(gm_core::error::GmError::Config(
                    "A preferred model requires a preferred provider".into(),
                ));
            }
            None => None,
        };
        Ok(self)
    }

    /// Select primary target and optional fallback target according to the rules table.
    pub fn route(&self, task: TaskType) -> (ModelTarget, Option<ModelTarget>) {
        let (mut primary, fallback) = match task {
            TaskType::CuVisionStep => (
                ModelTarget {
                    provider: ProviderKind::Meta,
                    api_base: self.config.meta_api_base.clone(),
                    model: self.config.meta_cu_model.clone(),
                    allow_vision: self.config.allow_screenshot_egress_meta,
                },
                Some(ModelTarget {
                    provider: ProviderKind::Xai,
                    api_base: self.config.xai_api_base.clone(),
                    model: self.config.xai_model.clone(),
                    allow_vision: self.config.allow_screenshot_egress_xai,
                }),
            ),
            TaskType::CuAxStep => (
                ModelTarget {
                    provider: ProviderKind::Meta,
                    api_base: self.config.meta_api_base.clone(),
                    model: self.config.meta_cu_model.clone(),
                    allow_vision: false,
                },
                Some(ModelTarget {
                    provider: ProviderKind::Xai,
                    api_base: self.config.xai_api_base.clone(),
                    model: self.config.xai_model.clone(),
                    allow_vision: false,
                }),
            ),
            TaskType::Planning => (
                ModelTarget {
                    provider: ProviderKind::Meta,
                    api_base: self.config.meta_api_base.clone(),
                    model: self.config.meta_reasoning_model.clone(),
                    allow_vision: false,
                },
                Some(ModelTarget {
                    provider: ProviderKind::Xai,
                    api_base: self.config.xai_api_base.clone(),
                    model: self.config.xai_model.clone(),
                    allow_vision: false,
                }),
            ),
            TaskType::ProactiveTriage => (
                ModelTarget {
                    provider: ProviderKind::Meta,
                    api_base: self.config.meta_api_base.clone(),
                    model: self.config.meta_cu_model.clone(),
                    allow_vision: false,
                },
                None,
            ),
            TaskType::LiveWebSearch => (
                ModelTarget {
                    provider: ProviderKind::Xai,
                    api_base: self.config.xai_api_base.clone(),
                    model: self.config.xai_model.clone(),
                    allow_vision: false,
                },
                Some(ModelTarget {
                    provider: ProviderKind::Meta,
                    api_base: self.config.meta_api_base.clone(),
                    model: self.config.meta_reasoning_model.clone(),
                    allow_vision: false,
                }),
            ),
        };
        if let Some((provider, model)) = &self.preference {
            primary = match provider {
                ProviderKind::Meta => ModelTarget {
                    provider: provider.clone(),
                    api_base: self.config.meta_api_base.clone(),
                    model: self.config.meta_cu_model.clone(),
                    allow_vision: task == TaskType::CuVisionStep
                        && self.config.allow_screenshot_egress_meta,
                },
                ProviderKind::Xai => ModelTarget {
                    provider: provider.clone(),
                    api_base: self.config.xai_api_base.clone(),
                    model: self.config.xai_model.clone(),
                    allow_vision: task == TaskType::CuVisionStep
                        && self.config.allow_screenshot_egress_xai,
                },
            };
            if let Some(model) = model {
                primary.model = model.clone();
            }
            // An explicit per-bot choice never silently changes provider.
            return (primary, None);
        }
        (primary, fallback)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_router_rules() {
        let router = LlmRouter::new(ProvidersConfig::default());
        let (primary, fallback) = router.route(TaskType::CuVisionStep);
        assert_eq!(primary.provider, ProviderKind::Meta);
        assert_eq!(primary.model, "muse-spark");
        assert!(fallback.is_some());
        assert_eq!(fallback.unwrap().provider, ProviderKind::Xai);

        let (primary, fallback) = router.route(TaskType::LiveWebSearch);
        assert_eq!(primary.provider, ProviderKind::Xai);
        assert_eq!(primary.model, "grok-4");
        assert!(fallback.is_some());
    }
}
