pub mod config;
pub mod device;
pub mod engine;
pub mod text;
pub mod tokenizer;
pub mod vision;
pub mod weights;

pub use config::*;
pub use device::*;
pub use engine::*;
pub use text::*;
pub use tokenizer::*;
pub use vision::*;
pub use weights::*;

#[cfg(test)]
mod tests {
    use super::*;
    use gm_llm::{LocalVlmBackend, LocalVlmLlmAdapter, VlmDevice};
    use std::sync::Arc;

    #[test]
    fn test_device_resolution_and_fallback() {
        // CPU resolution
        let cpu_ctx = VlmDeviceContext::resolve(VlmDevice::Cpu);
        assert_eq!(cpu_ctx.active_device_kind, VlmDevice::Cpu);
        assert!(!cpu_ctx.is_accelerated);

        // Metal resolution (should succeed or fall back to CPU safely without panic)
        let metal_ctx = VlmDeviceContext::resolve(VlmDevice::Metal);
        assert_eq!(metal_ctx.requested_device_kind, VlmDevice::Metal);
        assert!(
            metal_ctx.active_device_kind == VlmDevice::Metal
                || metal_ctx.active_device_kind == VlmDevice::Cpu
        );
    }

    #[test]
    fn test_vlm_tokenizer_formatting_and_decoding() {
        let smol_tok = VlmTokenizer::new(VlmModelArchitecture::SmolVlm);
        assert_eq!(smol_tok.special_tokens().image_token, "<image>");

        let prompt = smol_tok.format_grounding_prompt("Click Submit button");
        assert!(prompt.contains("<image>"));
        assert!(prompt.contains("Click Submit button"));

        let token_ids = smol_tok.encode("Click Submit button");
        assert!(!token_ids.is_empty());

        let decoded = smol_tok.decode(&token_ids);
        assert!(!decoded.is_empty());

        let qwen_tok = VlmTokenizer::new(VlmModelArchitecture::Qwen2Vl);
        assert_eq!(qwen_tok.special_tokens().image_token, "<|image_pad|>");
        let qwen_prompt = qwen_tok.format_triage_prompt("Check password prompt");
        assert!(qwen_prompt.contains("<|vision_start|>"));
    }

    #[tokio::test]
    async fn test_smolvlm_scaffolding_and_tensors() {
        let config = CandleVlmConfig::smolvlm_preset().with_device(VlmDevice::Cpu);
        let engine = CandleVlmEngine::new(config).unwrap();
        assert_eq!(engine.config().architecture, VlmModelArchitecture::SmolVlm);
        assert_eq!(engine.model_name(), "HuggingFaceTB/SmolVLM-Instruct");

        let dummy_jpeg = vec![0x80u8; 1024];
        let grounding = engine
            .ground(&dummy_jpeg, "Click Submit button")
            .await
            .unwrap();

        assert!(grounding.x > 0.0);
        assert!(grounding.y > 0.0);
        assert!(grounding.confidence >= 0.85);
        assert!(grounding.bbox.is_some());
        let bbox = grounding.bbox.unwrap();
        assert!(bbox.x_min < bbox.x_max);
        assert!(bbox.y_min < bbox.y_max);
    }

    #[tokio::test]
    async fn test_qwen2_vl_scaffolding_and_tensors() {
        let config = CandleVlmConfig::qwen2_vl_preset().with_device(VlmDevice::Cpu);
        let engine = CandleVlmEngine::new(config).unwrap();
        assert_eq!(engine.config().architecture, VlmModelArchitecture::Qwen2Vl);
        assert_eq!(engine.model_name(), "Qwen/Qwen2-VL-2B-Instruct");

        let dummy_jpeg = vec![0x40u8; 2048];
        let triage = engine
            .triage(&dummy_jpeg, "Screen check for password field")
            .await
            .unwrap();

        assert!(triage.contains_sensitive_data);
        assert_eq!(triage.label, "sensitive_input_detected");
        assert!(triage.confidence >= 0.85);
    }

    #[tokio::test]
    async fn test_safetensors_weights_save_load_roundtrip() {
        let temp_dir = tempfile::tempdir().unwrap();
        let weights_path = temp_dir.path().join("model.safetensors");

        let device = candle_core::Device::Cpu;
        let dtype = candle_core::DType::F32;

        let stats = SafetensorsWeightLoader::create_synthetic_weights_file(
            &weights_path,
            3 * 16 * 16,
            768,
            1024,
            49152,
            &device,
            dtype,
        )
        .unwrap();

        assert_eq!(stats.len(), 5);
        assert!(weights_path.exists());

        // Initialize engine pointing to the created safetensors weights
        let config = CandleVlmConfig::smolvlm_preset()
            .with_device(VlmDevice::Cpu)
            .with_weights_path(&weights_path);

        let engine = CandleVlmEngine::new(config).unwrap();
        let dummy_jpeg = vec![0x55u8; 1024];

        let gen_output = engine.generate(&dummy_jpeg, "Click Submit").await.unwrap();
        assert!(gen_output.contains("click"));
        assert!(gen_output.contains("params"));
    }

    #[tokio::test]
    async fn test_offline_visual_triage_states() {
        let config = CandleVlmConfig::smolvlm_preset().with_device(VlmDevice::Cpu);
        let engine = CandleVlmEngine::new(config).unwrap();
        let dummy_jpeg = vec![0x77u8; 1024];

        // 1. Password input -> sensitive
        let t1 = engine
            .triage(&dummy_jpeg, "Enter your administrator password")
            .await
            .unwrap();
        assert!(t1.contains_sensitive_data);
        assert!(!t1.is_terminal_state);

        // 2. Task complete -> terminal
        let t2 = engine
            .triage(&dummy_jpeg, "Task is done and completed")
            .await
            .unwrap();
        assert!(!t2.contains_sensitive_data);
        assert!(t2.is_terminal_state);
        assert_eq!(t2.label, "terminal_state");

        // 3. Normal screen -> ready
        let t3 = engine
            .triage(&dummy_jpeg, "Inspect main dashboard view")
            .await
            .unwrap();
        assert!(!t3.contains_sensitive_data);
        assert!(!t3.is_terminal_state);
        assert_eq!(t3.label, "screen_ready");
    }

    #[tokio::test]
    async fn test_offline_image_grounding_coordinates_and_bounds() {
        let config = CandleVlmConfig::smolvlm_preset().with_device(VlmDevice::Cpu);
        let engine = CandleVlmEngine::new(config).unwrap();
        let dummy_jpeg = vec![0xAAu8; 512];

        // Grounding Close button -> top left region
        let g_close = engine
            .ground(&dummy_jpeg, "Click the Close button")
            .await
            .unwrap();
        assert!(g_close.x < 300.0);
        assert!(g_close.y < 300.0);

        // Grounding Submit button -> bottom right region
        let g_submit = engine
            .ground(&dummy_jpeg, "Click the Submit button")
            .await
            .unwrap();
        assert!(g_submit.x > 600.0);
        assert!(g_submit.y > 600.0);

        // Verify bounding box properties
        let bbox = g_submit.bbox.unwrap();
        assert!(bbox.width() > 0.0);
        assert!(bbox.height() > 0.0);
        let (cx, cy) = bbox.center();
        assert!((0.0..=1.0).contains(&cx));
        assert!((0.0..=1.0).contains(&cy));
    }

    #[tokio::test]
    async fn test_local_vlm_backend_adapter_integration() {
        use gm_llm::{ChatMessage, LlmBackend, ModelTarget, ProviderKind};

        let config = CandleVlmConfig::smolvlm_preset().with_device(VlmDevice::Cpu);
        let engine = Arc::new(CandleVlmEngine::new(config).unwrap());
        let adapter = LocalVlmLlmAdapter::new(engine);

        let target = ModelTarget {
            provider: ProviderKind::Meta,
            api_base: "local://candle".into(),
            model: "HuggingFaceTB/SmolVLM-Instruct".into(),
            allow_vision: true,
        };

        let messages = vec![ChatMessage {
            role: "user".into(),
            content: "Click the Submit button".into(),
        }];

        let resp = adapter.complete(&target, &messages).await.unwrap();
        assert_eq!(resp.model, "HuggingFaceTB/SmolVLM-Instruct");
        assert_eq!(resp.estimated_cost_usd, 0.0);
        assert!(resp.content.contains("click"));
    }

    #[tokio::test]
    async fn test_empty_image_error_handling() {
        let config = CandleVlmConfig::smolvlm_preset().with_device(VlmDevice::Cpu);
        let engine = CandleVlmEngine::new(config).unwrap();

        let triage_err = engine.triage(&[], "test prompt").await;
        assert!(
            triage_err.is_err(),
            "Empty image buffer must return an error"
        );

        let ground_err = engine.ground(&[], "test prompt").await;
        assert!(
            ground_err.is_err(),
            "Empty image buffer must return an error"
        );
    }

    #[test]
    fn test_zero_dimension_and_patch_size_validation() {
        let mut invalid_patch = CandleVlmConfig::smolvlm_preset();
        invalid_patch.patch_size = 0;
        assert!(invalid_patch.validate().is_err());
        assert!(CandleVlmEngine::new(invalid_patch).is_err());

        let mut invalid_dims = CandleVlmConfig::smolvlm_preset();
        invalid_dims.image_size = (300, 400); // 300 not divisible by 16
        assert!(invalid_dims.validate().is_err());
        assert!(CandleVlmEngine::new(invalid_dims).is_err());

        let mut zero_dim = CandleVlmConfig::smolvlm_preset();
        zero_dim.vision_dim = 0;
        assert!(zero_dim.validate().is_err());
    }

    #[tokio::test]
    async fn test_nonexistent_weights_path_error() {
        let config = CandleVlmConfig::smolvlm_preset()
            .with_device(VlmDevice::Cpu)
            .with_weights_path("/nonexistent/path/to/model.safetensors");

        let res = CandleVlmEngine::new(config);
        assert!(
            res.is_err(),
            "Non-existent weights file must return an error"
        );
    }

    #[tokio::test]
    async fn test_safetensors_tensor_shape_mismatch_verification() {
        let temp_dir = tempfile::tempdir().unwrap();
        let weights_path = temp_dir.path().join("mismatched_model.safetensors");

        // Write a tensor with invalid/mismatched shape for patch_projection (e.g. [10, 10])
        let device = candle_core::Device::Cpu;
        let mut map = std::collections::HashMap::new();
        let bad_patch =
            candle_core::Tensor::zeros((10, 10), candle_core::DType::F32, &device).unwrap();
        let mm_proj =
            candle_core::Tensor::zeros((768, 1024), candle_core::DType::F32, &device).unwrap();
        let tok_embed =
            candle_core::Tensor::zeros((49152, 1024), candle_core::DType::F32, &device).unwrap();
        let attn_w =
            candle_core::Tensor::zeros((1024, 1024), candle_core::DType::F32, &device).unwrap();
        let lm_head =
            candle_core::Tensor::zeros((1024, 49152), candle_core::DType::F32, &device).unwrap();

        map.insert("vision_model.patch_projection.weight".into(), bad_patch);
        map.insert("vision_model.multimodal_projector.weight".into(), mm_proj);
        map.insert("language_model.embed_tokens.weight".into(), tok_embed);
        map.insert("language_model.attention.weight".into(), attn_w);
        map.insert("language_model.lm_head.weight".into(), lm_head);

        SafetensorsWeightLoader::save_safetensors(&map, &weights_path).unwrap();

        let config = CandleVlmConfig::smolvlm_preset()
            .with_device(VlmDevice::Cpu)
            .with_weights_path(&weights_path);

        let res = CandleVlmEngine::new(config);
        assert!(
            res.is_err(),
            "Mismatched tensor shape must fail verification"
        );
        let err_msg = res.err().unwrap().to_string();
        assert!(err_msg.contains("Tensor shape mismatch"));
    }

    #[tokio::test]
    async fn test_safetensors_corrupted_file_detection() {
        let temp_dir = tempfile::tempdir().unwrap();
        let corrupt_path = temp_dir.path().join("corrupted.safetensors");
        std::fs::write(&corrupt_path, b"not_a_valid_safetensors_file_data").unwrap();

        let config = CandleVlmConfig::smolvlm_preset()
            .with_device(VlmDevice::Cpu)
            .with_weights_path(&corrupt_path);

        let res = CandleVlmEngine::new(config);
        assert!(
            res.is_err(),
            "Corrupted safetensors file must return an error"
        );
    }

    #[test]
    fn test_enhanced_grounding_coordinate_parsing() {
        let tok = VlmTokenizer::new(VlmModelArchitecture::Qwen2Vl);

        // 1. JSON action params (generated by CandleVlmEngine::generate)
        let action_json = r#"{"action":"click","params":{"x":750.0,"y":500.0,"confidence":0.95}}"#;
        let parsed = tok.extract_grounding_coordinates(action_json).unwrap();
        assert_eq!(parsed.0, 750.0);
        assert_eq!(parsed.1, 500.0);

        // 2. JSON array box [ymin, xmin, ymax, xmax] (Qwen2-VL format)
        let array_box_json = r#"{"point": [100.0, 200.0], "box_2d": [180, 80, 220, 120]}"#;
        let parsed_box = tok.extract_grounding_coordinates(array_box_json).unwrap();
        assert_eq!(parsed_box.0, 100.0);
        assert_eq!(parsed_box.1, 200.0);
        assert!(parsed_box.2.is_some());
        let bbox = parsed_box.2.unwrap();
        assert_eq!(bbox.x_min, 0.08); // 80 / 1000
        assert_eq!(bbox.y_min, 0.18); // 180 / 1000

        // 3. Parentheses coordinate pattern (x, y)
        let parens_str = "Selected point is (150.0, 250.0)";
        let parsed_p = tok.extract_grounding_coordinates(parens_str).unwrap();
        assert_eq!(parsed_p.0, 150.0);
        assert_eq!(parsed_p.1, 250.0);

        // 4. Qwen2-VL box tokens
        let token_str = "Detected element <|box_start|>(180,80),(220,120)<|box_end|>";
        let parsed_t = tok.extract_grounding_coordinates(token_str).unwrap();
        assert_eq!(parsed_t.0, 100.0); // Center of x: (80 + 120)/2 = 100
        assert_eq!(parsed_t.1, 160.0); // Center of y: (180 + 220)/2 * 800 / 1000 = 160
    }
}
