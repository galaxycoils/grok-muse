use candle_core::{DType, Device, IndexOp, Result as CandleResult, Tensor};
use candle_transformers::generation::LogitsProcessor;

/// Language model decoder for SmolVLM and Qwen2-VL transformer layers.
#[derive(Debug, Clone)]
pub struct LanguageModel {
    pub vocab_size: usize,
    pub text_dim: usize,
    pub num_layers: usize,
    pub num_heads: usize,
    pub token_embedding: Tensor,
    pub attention_weight: Tensor,
    pub lm_head: Tensor,
}

impl LanguageModel {
    pub fn new(
        vocab_size: usize,
        text_dim: usize,
        num_layers: usize,
        num_heads: usize,
        device: &Device,
        dtype: DType,
    ) -> CandleResult<Self> {
        if vocab_size == 0 || text_dim == 0 {
            return Err(candle_core::Error::Msg(
                "vocab_size and text_dim must be greater than zero".into(),
            ));
        }
        if num_layers == 0 || num_heads == 0 {
            return Err(candle_core::Error::Msg(
                "num_layers and num_heads must be greater than zero".into(),
            ));
        }
        let token_embedding = Tensor::zeros((vocab_size, text_dim), dtype, device)?;
        let attention_weight = Tensor::zeros((text_dim, text_dim), dtype, device)?;
        let lm_head = Tensor::zeros((text_dim, vocab_size), dtype, device)?;

        Ok(Self {
            vocab_size,
            text_dim,
            num_layers,
            num_heads,
            token_embedding,
            attention_weight,
            lm_head,
        })
    }

    /// Embed token IDs into continuous embedding vectors `[1, seq_len, text_dim]`.
    pub fn embed_tokens(&self, token_ids: &[u32], device: &Device) -> CandleResult<Tensor> {
        let seq_len = token_ids.len();
        if seq_len == 0 {
            return Tensor::zeros((1, 0, self.text_dim), self.token_embedding.dtype(), device);
        }

        let clamped: Vec<u32> = token_ids
            .iter()
            .map(|&id| id.min((self.vocab_size.saturating_sub(1)) as u32))
            .collect();
        let id_tensor = Tensor::from_slice(&clamped, (seq_len,), device)?;
        let embeds = self.token_embedding.index_select(&id_tensor, 0)?;
        embeds.reshape((1, seq_len, self.text_dim))
    }

    /// Forward pass through transformer layer and LM head projection:
    /// `hidden_states [batch, seq_len, text_dim]` -> `logits [batch, seq_len, vocab_size]`.
    pub fn forward(&self, hidden_states: &Tensor) -> CandleResult<Tensor> {
        let (batch, seq_len, dim) = hidden_states.dims3()?;

        // 1. Attention projection: [batch * seq_len, text_dim] x [text_dim, text_dim]
        let flat = hidden_states.reshape((batch * seq_len, dim))?;
        let attn_out = flat.matmul(&self.attention_weight)?;

        // 2. Residual connection + simple normalization approximation
        let combined = (&flat + &attn_out)?;

        // 3. LM head projection -> [batch * seq_len, vocab_size]
        let logits_flat = combined.matmul(&self.lm_head)?;

        // 4. Reshape to [batch, seq_len, vocab_size]
        logits_flat.reshape((batch, seq_len, self.vocab_size))
    }

    /// Autoregressive token generation using `candle-transformers` `LogitsProcessor`.
    pub fn generate_autoregressive(
        &self,
        initial_embeds: &Tensor,
        max_new_tokens: usize,
        eos_token_id: u32,
        temperature: f32,
        device: &Device,
    ) -> CandleResult<Vec<u32>> {
        let temp_opt = if temperature <= 0.01 {
            None
        } else {
            Some(temperature as f64)
        };
        let mut logits_processor = LogitsProcessor::new(42, temp_opt, None);
        let mut current_seq = initial_embeds.clone();
        let mut generated = Vec::with_capacity(max_new_tokens);

        for _ in 0..max_new_tokens {
            let (_, seq_len, _) = current_seq.dims3()?;
            if seq_len == 0 {
                break;
            }
            let logits = self.forward(&current_seq)?;
            let last_logits = logits.i((0, seq_len - 1))?.to_dtype(DType::F32)?;
            let next_token = logits_processor.sample(&last_logits)?;
            if next_token == eos_token_id {
                break;
            }
            generated.push(next_token);
            let next_embed = self.embed_tokens(&[next_token], device)?;
            current_seq = Tensor::cat(&[&current_seq, &next_embed], 1)?;
        }

        Ok(generated)
    }
}
