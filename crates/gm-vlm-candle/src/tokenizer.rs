use crate::config::VlmModelArchitecture;
use gm_llm::VlmBoundingBox;
use std::collections::HashMap;

/// Special token indices and symbols for SmolVLM and Qwen2-VL architectures.
#[derive(Debug, Clone)]
pub struct VlmSpecialTokens {
    pub image_token: &'static str,
    pub image_token_id: u32,
    pub bos_token: &'static str,
    pub bos_token_id: u32,
    pub eos_token: &'static str,
    pub eos_token_id: u32,
    pub pad_token_id: u32,
}

/// Tokenizer scaffolding supporting SmolVLM and Qwen2-VL tokenization schemes,
/// image token injection, and structured UI coordinate parsing.
#[derive(Debug, Clone)]
pub struct VlmTokenizer {
    architecture: VlmModelArchitecture,
    vocab: HashMap<String, u32>,
    inv_vocab: HashMap<u32, String>,
    special_tokens: VlmSpecialTokens,
}

impl VlmTokenizer {
    pub fn new(architecture: VlmModelArchitecture) -> Self {
        let (image_token, image_token_id, bos_token, bos_token_id, eos_token, eos_token_id) =
            match architecture {
                VlmModelArchitecture::SmolVlm => {
                    ("<image>", 49150, "<|im_start|>", 49151, "<|im_end|>", 49152)
                }
                VlmModelArchitecture::Qwen2Vl => (
                    "<|image_pad|>",
                    151655,
                    "<|im_start|>",
                    151644,
                    "<|im_end|>",
                    151645,
                ),
            };

        let mut vocab = HashMap::new();
        let mut inv_vocab = HashMap::new();

        // Seed basic vocabulary and special tokens
        let specials = [
            (image_token, image_token_id),
            (bos_token, bos_token_id),
            (eos_token, eos_token_id),
            ("<pad>", 0),
            ("<unk>", 1),
            ("<box_start>", 2),
            ("<box_end>", 3),
            ("[click]", 4),
            ("[type]", 5),
            ("[done]", 6),
            ("[wait]", 7),
        ];

        for (tok, id) in specials {
            vocab.insert(tok.to_string(), id);
            inv_vocab.insert(id, tok.to_string());
        }

        Self {
            architecture,
            vocab,
            inv_vocab,
            special_tokens: VlmSpecialTokens {
                image_token,
                image_token_id,
                bos_token,
                bos_token_id,
                eos_token,
                eos_token_id,
                pad_token_id: 0,
            },
        }
    }

    pub fn architecture(&self) -> VlmModelArchitecture {
        self.architecture
    }

    pub fn special_tokens(&self) -> &VlmSpecialTokens {
        &self.special_tokens
    }

    /// Format prompt for visual grounding according to model template.
    pub fn format_grounding_prompt(&self, instruction: &str) -> String {
        match self.architecture {
            VlmModelArchitecture::SmolVlm => {
                format!(
                    "<|im_start|>system\nYou are a desktop GUI grounding agent. Locate the target element and return normalized coordinates and bounding box in JSON format.<|im_end|>\n<|im_start|>user\n<image>\nLocate target element: {instruction}<|im_end|>\n<|im_start|>assistant\n"
                )
            }
            VlmModelArchitecture::Qwen2Vl => {
                format!(
                    "<|im_start|>system\nYou are a desktop vision assistant that outputs bounding boxes for GUI elements.<|im_end|>\n<|im_start|>user\n<|vision_start|><|image_pad|><|vision_end|>Locate: {instruction}<|im_end|>\n<|im_start|>assistant\n"
                )
            }
        }
    }

    /// Format prompt for visual triage according to model template.
    pub fn format_triage_prompt(&self, prompt: &str) -> String {
        match self.architecture {
            VlmModelArchitecture::SmolVlm => {
                format!(
                    "<|im_start|>system\nYou are a visual safety and state classifier. Inspect the screen and classify modality, sensitive inputs (passwords/credentials), and terminal states.<|im_end|>\n<|im_start|>user\n<image>\n{prompt}<|im_end|>\n<|im_start|>assistant\n"
                )
            }
            VlmModelArchitecture::Qwen2Vl => {
                format!(
                    "<|im_start|>system\nYou are a desktop visual safety classifier.<|im_end|>\n<|im_start|>user\n<|vision_start|><|image_pad|><|vision_end|>{prompt}<|im_end|>\n<|im_start|>assistant\n"
                )
            }
        }
    }

    /// Tokenize text into sequence of token IDs.
    pub fn encode(&self, text: &str) -> Vec<u32> {
        let mut tokens = Vec::new();
        let words: Vec<&str> = text.split_whitespace().collect();

        for word in words {
            if let Some(&id) = self.vocab.get(word) {
                tokens.push(id);
            } else {
                // Deterministic subword hash mapping for testing & scaffolding
                let hash = word
                    .bytes()
                    .fold(17u32, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u32));
                let token_id = 100 + (hash % 1000);
                tokens.push(token_id);
            }
        }

        tokens
    }

    /// Decode sequence of token IDs into text.
    pub fn decode(&self, token_ids: &[u32]) -> String {
        let mut words = Vec::new();
        for &id in token_ids {
            if let Some(word) = self.inv_vocab.get(&id) {
                words.push(word.clone());
            } else {
                words.push(format!("tok_{id}"));
            }
        }
        words.join(" ")
    }

    /// Parse grounding coordinates or bounding box from model output text.
    pub fn extract_grounding_coordinates(
        &self,
        text: &str,
    ) -> Option<(f64, f64, Option<VlmBoundingBox>)> {
        // Helper to parse a bounding box from JSON Value (supporting both object and array formats)
        fn parse_bbox_val(val: &serde_json::Value) -> Option<VlmBoundingBox> {
            if let Some(arr) = val.as_array()
                && arr.len() >= 4
            {
                let mut v = [0.0f64; 4];
                for (i, item) in arr.iter().take(4).enumerate() {
                    v[i] = item.as_f64()?;
                }
                // Check if 0..1000 integer coordinate scale (e.g. Qwen2-VL / SmolVLM box tokens)
                let scale = if v.iter().any(|&c| c > 1.0) {
                    1000.0
                } else {
                    1.0
                };
                // Format: [ymin, xmin, ymax, xmax]
                let (y_min, x_min, y_max, x_max) =
                    (v[0] / scale, v[1] / scale, v[2] / scale, v[3] / scale);
                return Some(VlmBoundingBox {
                    x_min: x_min.min(x_max),
                    y_min: y_min.min(y_max),
                    x_max: x_min.max(x_max),
                    y_max: y_min.max(y_max),
                });
            }
            if val.is_object()
                && let Ok(b) = serde_json::from_value::<VlmBoundingBox>(val.clone())
            {
                return Some(b);
            }
            None
        }

        // 1. Attempt parsing JSON format:
        // {"point": [x, y], "box_2d": [ymin, xmin, ymax, xmax]} or
        // {"action":"click", "params":{"x": x, "y": y, "bbox": ...}}
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(text) {
            let params = val.get("params");
            let target_ctx = params.unwrap_or(&val);

            let mut parsed_x = None;
            let mut parsed_y = None;

            if let Some(arr) = target_ctx
                .get("point")
                .or_else(|| target_ctx.get("coords"))
                .and_then(|p| p.as_array())
                && arr.len() >= 2
            {
                parsed_x = arr[0].as_f64();
                parsed_y = arr[1].as_f64();
            } else if let (Some(x_val), Some(y_val)) = (
                target_ctx.get("x").and_then(|v| v.as_f64()),
                target_ctx.get("y").and_then(|v| v.as_f64()),
            ) {
                parsed_x = Some(x_val);
                parsed_y = Some(y_val);
            }

            let bbox = target_ctx
                .get("bbox")
                .or_else(|| target_ctx.get("box_2d"))
                .or_else(|| val.get("bbox"))
                .or_else(|| val.get("box_2d"))
                .and_then(parse_bbox_val);

            // If point coordinates weren't explicit but bbox was provided, infer from box center
            if parsed_x.is_none()
                && let Some(ref b) = bbox
            {
                let (cx, cy) = b.center();
                parsed_x = Some(cx);
                parsed_y = Some(cy);
            }

            if let (Some(x), Some(y)) = (parsed_x, parsed_y) {
                return Some((x, y, bbox));
            }
        }

        // 2. Qwen2-VL special box token syntax: <|box_start|>(ymin,xmin),(ymax,xmax)<|box_end|>
        if let Some(start) = text.find("<|box_start|>")
            && let Some(end) = text.find("<|box_end|>")
        {
            let inner = &text[start + "<|box_start|>".len()..end];
            let clean = inner.replace(['(', ')'], "");
            let parts: Vec<&str> = clean.split(',').collect();
            if parts.len() >= 4
                && let (Ok(y1), Ok(x1), Ok(y2), Ok(x2)) = (
                    parts[0].trim().parse::<f64>(),
                    parts[1].trim().parse::<f64>(),
                    parts[2].trim().parse::<f64>(),
                    parts[3].trim().parse::<f64>(),
                )
            {
                let scale = if y1 > 1.0 || x1 > 1.0 || y2 > 1.0 || x2 > 1.0 {
                    1000.0
                } else {
                    1.0
                };
                let bbox = VlmBoundingBox {
                    x_min: (x1 / scale).min(x2 / scale),
                    y_min: (y1 / scale).min(y2 / scale),
                    x_max: (x1 / scale).max(x2 / scale),
                    y_max: (y1 / scale).max(y2 / scale),
                };
                let (cx, cy) = bbox.center();
                return Some((cx * 1000.0, cy * 800.0, Some(bbox)));
            }
        }

        // 3. Fallback: look for coordinate patterns like [x, y] or (x, y)
        for (open, close) in [('[', ']'), ('(', ')')] {
            if let Some(start) = text.find(open)
                && let Some(end) = text[start..].find(close)
            {
                let inner = &text[start + 1..start + end];
                let parts: Vec<&str> = inner.split(',').collect();
                if parts.len() == 2
                    && let (Ok(x), Ok(y)) = (
                        parts[0].trim().parse::<f64>(),
                        parts[1].trim().parse::<f64>(),
                    )
                {
                    return Some((x, y, None));
                }
            }
        }

        None
    }
}
