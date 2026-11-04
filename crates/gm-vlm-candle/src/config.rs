use gm_llm::{VlmDevice, VlmPrecision};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Supported local open-weight vision-language model architectures (Action A12).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum VlmModelArchitecture {
    #[default]
    SmolVlm,
    Qwen2Vl,
}

impl std::fmt::Display for VlmModelArchitecture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SmolVlm => write!(f, "SmolVLM"),
            Self::Qwen2Vl => write!(f, "Qwen2-VL"),
        }
    }
}

/// Execution mode for `CandleVlmEngine`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum VlmBackendMode {
    /// Automatically run real autoregressive weights when loaded, or calibrated spatial fallback otherwise.
    #[default]
    Auto,
    /// Require real non-zero checkpoint weights and HuggingFace tokenizer vocabulary.
    RealWeights,
    /// Deterministic fast offline mode for CI unit tests.
    SyntheticTest,
}

/// Configuration settings for the Candle-powered local VLM engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandleVlmConfig {
    pub architecture: VlmModelArchitecture,
    pub model_id: String,
    pub weights_path: Option<PathBuf>,
    #[serde(default)]
    pub shard_paths: Vec<PathBuf>,
    #[serde(default)]
    pub tokenizer_path: Option<PathBuf>,
    #[serde(default)]
    pub backend_mode: VlmBackendMode,
    pub device: VlmDevice,
    pub precision: VlmPrecision,
    pub image_size: (usize, usize),
    pub patch_size: usize,
    pub vision_dim: usize,
    pub text_dim: usize,
    pub vocab_size: usize,
    pub num_layers: usize,
    pub num_heads: usize,
    pub max_context: usize,
    pub temperature: f32,
}

impl Default for CandleVlmConfig {
    fn default() -> Self {
        Self::qwen2_vl_preset()
    }
}

impl CandleVlmConfig {
    /// Recommended SmolVLM configuration preset (lightweight, highly optimized for Apple Silicon).
    pub fn smolvlm_preset() -> Self {
        Self {
            architecture: VlmModelArchitecture::SmolVlm,
            model_id: "HuggingFaceTB/SmolVLM-Instruct".into(),
            weights_path: None,
            shard_paths: Vec::new(),
            tokenizer_path: None,
            backend_mode: VlmBackendMode::Auto,
            device: if cfg!(target_os = "macos") {
                VlmDevice::Metal
            } else {
                VlmDevice::Cpu
            },
            precision: VlmPrecision::Fp16,
            image_size: (384, 384),
            patch_size: 16,
            vision_dim: 768,
            text_dim: 1024,
            vocab_size: 49152,
            num_layers: 16,
            num_heads: 16,
            max_context: 4096,
            temperature: 0.1,
        }
    }

    /// SmolVLM2-2.2B-Instruct preset (second-generation compact multimodal model).
    pub fn smolvlm2_preset() -> Self {
        Self {
            architecture: VlmModelArchitecture::SmolVlm,
            model_id: "HuggingFaceTB/SmolVLM2-2.2B-Instruct".into(),
            weights_path: None,
            shard_paths: Vec::new(),
            tokenizer_path: None,
            backend_mode: VlmBackendMode::Auto,
            device: if cfg!(target_os = "macos") {
                VlmDevice::Metal
            } else {
                VlmDevice::Cpu
            },
            precision: VlmPrecision::Bf16,
            image_size: (384, 384),
            patch_size: 16,
            vision_dim: 1152,
            text_dim: 2048,
            vocab_size: 49280,
            num_layers: 24,
            num_heads: 32,
            max_context: 8192,
            temperature: 0.1,
        }
    }

    /// Qwen2-VL-2B configuration preset (default `v0.2.0` local VLM for OCR & desktop UI grounding).
    pub fn qwen2_vl_preset() -> Self {
        Self {
            architecture: VlmModelArchitecture::Qwen2Vl,
            model_id: "Qwen/Qwen2-VL-2B-Instruct".into(),
            weights_path: None,
            shard_paths: Vec::new(),
            tokenizer_path: None,
            backend_mode: VlmBackendMode::Auto,
            device: if cfg!(target_os = "macos") {
                VlmDevice::Metal
            } else {
                VlmDevice::Cpu
            },
            precision: VlmPrecision::Bf16,
            image_size: (448, 448),
            patch_size: 14,
            vision_dim: 1024,
            text_dim: 1536,
            vocab_size: 151936,
            num_layers: 24,
            num_heads: 12,
            max_context: 8192,
            temperature: 0.1,
        }
    }

    /// Resolve a `CandleVlmConfig` preset from a model ID or short alias.
    pub fn from_model_id(model_or_alias: &str) -> Option<Self> {
        let trimmed = model_or_alias.trim();
        let lower = trimmed.to_ascii_lowercase();
        match lower.as_str() {
            "qwen2-vl-2b" | "qwen2-vl" | "qwen/qwen2-vl-2b-instruct" => {
                Some(Self::qwen2_vl_preset())
            }
            "smolvlm2-2.2b" | "smolvlm2" | "huggingfacetb/smolvlm2-2.2b-instruct" => {
                Some(Self::smolvlm2_preset())
            }
            "smolvlm" | "huggingfacetb/smolvlm-instruct" => Some(Self::smolvlm_preset()),
            _ => None,
        }
    }

    pub fn with_weights_path(mut self, path: impl AsRef<Path>) -> Self {
        self.weights_path = Some(path.as_ref().to_path_buf());
        self
    }

    pub fn with_shard_paths(mut self, paths: Vec<PathBuf>) -> Self {
        self.shard_paths = paths;
        self
    }

    pub fn with_tokenizer_path(mut self, path: impl AsRef<Path>) -> Self {
        self.tokenizer_path = Some(path.as_ref().to_path_buf());
        self
    }

    pub fn with_backend_mode(mut self, mode: VlmBackendMode) -> Self {
        self.backend_mode = mode;
        self
    }

    pub fn with_device(mut self, device: VlmDevice) -> Self {
        self.device = device;
        self
    }

    pub fn with_precision(mut self, precision: VlmPrecision) -> Self {
        self.precision = precision;
        self
    }

    /// Validate configuration constraints and dimension alignments.
    pub fn validate(&self) -> candle_core::Result<()> {
        if self.patch_size == 0 {
            return Err(candle_core::Error::Msg(
                "patch_size must be greater than zero".into(),
            ));
        }
        let (img_h, img_w) = self.image_size;
        if img_h == 0 || img_w == 0 {
            return Err(candle_core::Error::Msg(
                "image_size dimensions must be greater than zero".into(),
            ));
        }
        if img_h % self.patch_size != 0 || img_w % self.patch_size != 0 {
            return Err(candle_core::Error::Msg(format!(
                "Image dimensions ({img_h}x{img_w}) must be divisible by patch_size ({})",
                self.patch_size
            )));
        }
        if self.vision_dim == 0 || self.text_dim == 0 || self.vocab_size == 0 {
            return Err(candle_core::Error::Msg(
                "vision_dim, text_dim, and vocab_size must be greater than zero".into(),
            ));
        }
        Ok(())
    }
}
