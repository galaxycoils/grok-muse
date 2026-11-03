use candle_core::{DType, Device, Result as CandleResult, Tensor};
use std::collections::HashMap;
use std::path::Path;
use tracing::info;

/// Verification statistics for loaded model tensor weights.
#[derive(Debug, Clone, PartialEq)]
pub struct TensorWeightStats {
    pub name: String,
    pub shape: Vec<usize>,
    pub dtype: DType,
    pub element_count: usize,
}

/// Safetensors weight loader and verification harness (Action A12).
pub struct SafetensorsWeightLoader;

impl SafetensorsWeightLoader {
    /// Save a collection of Candle tensors to a standard Safetensors file on disk.
    pub fn save_safetensors(
        tensors: &HashMap<String, Tensor>,
        file_path: impl AsRef<Path>,
    ) -> CandleResult<()> {
        let p = file_path.as_ref();
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        candle_core::safetensors::save(tensors, p)?;
        info!(
            "Saved {} tensors to safetensors file: {:?}",
            tensors.len(),
            p
        );
        Ok(())
    }

    /// Load and verify tensors from a Safetensors file on disk to the target execution device.
    pub fn load_and_verify(
        file_path: impl AsRef<Path>,
        device: &Device,
    ) -> CandleResult<(HashMap<String, Tensor>, Vec<TensorWeightStats>)> {
        let p = file_path.as_ref();
        let tensors = candle_core::safetensors::load(p, device)?;

        let mut stats = Vec::new();
        for (name, tensor) in &tensors {
            let shape = tensor.dims().to_vec();
            let element_count = tensor.elem_count();
            let dtype = tensor.dtype();

            stats.push(TensorWeightStats {
                name: name.clone(),
                shape,
                dtype,
                element_count,
            });
        }

        info!(
            "Successfully loaded and verified {} safetensors weights from {:?}",
            stats.len(),
            p
        );
        Ok((tensors, stats))
    }

    /// Verify loaded tensors against expected model configuration dimensions (Action A12).
    pub fn verify_tensors_for_config(
        tensors: &HashMap<String, Tensor>,
        config: &crate::config::CandleVlmConfig,
    ) -> CandleResult<Vec<TensorWeightStats>> {
        let patch_vec_len = 3 * config.patch_size * config.patch_size;

        let expected_layers: [(&[&str], Vec<usize>); 5] = [
            (
                &[
                    "vision_model.patch_projection.weight",
                    "visual.patch_embed.proj.weight",
                    "patch_projection.weight",
                ],
                vec![patch_vec_len, config.vision_dim],
            ),
            (
                &[
                    "vision_model.multimodal_projector.weight",
                    "visual.merger.linear.weight",
                    "multimodal_projector.weight",
                ],
                vec![config.vision_dim, config.text_dim],
            ),
            (
                &[
                    "language_model.embed_tokens.weight",
                    "model.embed_tokens.weight",
                    "embed_tokens.weight",
                ],
                vec![config.vocab_size, config.text_dim],
            ),
            (
                &[
                    "language_model.attention.weight",
                    "model.layers.0.self_attn.q_proj.weight",
                    "attention.weight",
                ],
                vec![config.text_dim, config.text_dim],
            ),
            (
                &["language_model.lm_head.weight", "lm_head.weight"],
                vec![config.text_dim, config.vocab_size],
            ),
        ];

        let mut verified_stats = Vec::new();

        for (names, expected_shape) in &expected_layers {
            let found_tensor = names.iter().find_map(|name| tensors.get(*name));
            match found_tensor {
                Some(tensor) => {
                    let actual_shape = tensor.dims().to_vec();
                    if actual_shape != *expected_shape {
                        return Err(candle_core::Error::Msg(format!(
                            "Tensor shape mismatch for layer '{}': expected {:?}, found {:?}",
                            names[0], expected_shape, actual_shape
                        )));
                    }
                    if tensor.elem_count() == 0 {
                        return Err(candle_core::Error::Msg(format!(
                            "Tensor '{}' has zero elements",
                            names[0]
                        )));
                    }
                    verified_stats.push(TensorWeightStats {
                        name: names[0].to_string(),
                        shape: actual_shape,
                        dtype: tensor.dtype(),
                        element_count: tensor.elem_count(),
                    });
                }
                None => {
                    return Err(candle_core::Error::Msg(format!(
                        "Missing required tensor weight for layer (expected one of: {:?})",
                        names
                    )));
                }
            }
        }

        Ok(verified_stats)
    }

    /// Create and write a calibrated synthetic Safetensors model package for offline verification.
    pub fn create_synthetic_weights_file(
        file_path: impl AsRef<Path>,
        patch_vec_len: usize,
        vision_dim: usize,
        text_dim: usize,
        vocab_size: usize,
        device: &Device,
        dtype: DType,
    ) -> CandleResult<Vec<TensorWeightStats>> {
        let mut map = HashMap::new();

        let patch_proj = Tensor::zeros((patch_vec_len, vision_dim), dtype, device)?;
        let mm_proj = Tensor::zeros((vision_dim, text_dim), dtype, device)?;
        let tok_embed = Tensor::zeros((vocab_size, text_dim), dtype, device)?;
        let attn_w = Tensor::zeros((text_dim, text_dim), dtype, device)?;
        let lm_head = Tensor::zeros((text_dim, vocab_size), dtype, device)?;

        map.insert("vision_model.patch_projection.weight".into(), patch_proj);
        map.insert("vision_model.multimodal_projector.weight".into(), mm_proj);
        map.insert("language_model.embed_tokens.weight".into(), tok_embed);
        map.insert("language_model.attention.weight".into(), attn_w);
        map.insert("language_model.lm_head.weight".into(), lm_head);

        Self::save_safetensors(&map, file_path.as_ref())?;

        let (_, stats) = Self::load_and_verify(file_path.as_ref(), device)?;
        Ok(stats)
    }
}
