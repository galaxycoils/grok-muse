use crate::config::{CandleVlmConfig, VlmModelArchitecture};
use crate::engine::CandleVlmEngine;
use futures::StreamExt;
use gm_core::config::GmConfig;
use gm_core::error::{GmError, Result};
use gm_core::hash::compute_sha256;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use tracing::info;

pub const DEFAULT_LOCAL_VLM_MODEL_ID: &str = "Qwen/Qwen2-VL-2B-Instruct";
pub const DEFAULT_HF_BASE_URL: &str = "https://huggingface.co";

/// Curated open-weight VLM catalog entry for `gm models`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCatalogEntry {
    pub model_id: String,
    pub alias: String,
    pub architecture: VlmModelArchitecture,
    pub approx_size_gb: f32,
    pub is_default: bool,
    pub description: String,
}

/// On-disk manifest stored inside `<models_dir>/<sanitized_id>/manifest.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledModelManifest {
    pub model_id: String,
    pub alias: String,
    pub architecture: VlmModelArchitecture,
    pub pulled_at: String,
    pub files: Vec<DownloadedModelFile>,
    pub total_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadedModelFile {
    pub filename: String,
    pub size_bytes: u64,
    pub sha256: String,
}

/// Status row returned by `ModelRegistry::list_models`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalModelStatus {
    pub model_id: String,
    pub alias: String,
    pub architecture: VlmModelArchitecture,
    pub is_default: bool,
    pub installed: bool,
    pub has_weights: bool,
    pub has_tokenizer: bool,
    pub size_bytes: u64,
    pub install_dir: PathBuf,
    pub description: String,
}

/// Local VLM Registry managing `~/Library/Application Support/GrokMuse/models/`.
#[derive(Debug, Clone)]
pub struct ModelRegistry {
    models_dir: PathBuf,
}

impl ModelRegistry {
    pub fn default_registry() -> Self {
        if let Ok(override_dir) = std::env::var("GM_MODELS_DIR")
            && !override_dir.trim().is_empty()
        {
            return Self::new(override_dir);
        }
        let cfg = GmConfig::load_or_default(None);
        Self::new(cfg.storage_dir.join("models"))
    }

    pub fn new(models_dir: impl Into<PathBuf>) -> Self {
        Self {
            models_dir: models_dir.into(),
        }
    }

    pub fn models_dir(&self) -> &Path {
        &self.models_dir
    }

    /// Built-in catalog of supported local VLMs (`Qwen/Qwen2-VL-2B-Instruct` default).
    pub fn catalog() -> Vec<ModelCatalogEntry> {
        vec![
            ModelCatalogEntry {
                model_id: DEFAULT_LOCAL_VLM_MODEL_ID.into(),
                alias: "qwen2-vl-2b".into(),
                architecture: VlmModelArchitecture::Qwen2Vl,
                approx_size_gb: 4.4,
                is_default: true,
                description: "Default v0.2.0 local VLM: high-accuracy desktop OCR & SoM grounding"
                    .into(),
            },
            ModelCatalogEntry {
                model_id: "HuggingFaceTB/SmolVLM2-2.2B-Instruct".into(),
                alias: "smolvlm2-2.2b".into(),
                architecture: VlmModelArchitecture::SmolVlm,
                approx_size_gb: 4.5,
                is_default: false,
                description: "Compact second-gen SmolVLM2 optimized for Apple Silicon Metal".into(),
            },
            ModelCatalogEntry {
                model_id: "HuggingFaceTB/SmolVLM-Instruct".into(),
                alias: "smolvlm".into(),
                architecture: VlmModelArchitecture::SmolVlm,
                approx_size_gb: 3.6,
                is_default: false,
                description: "Lightweight SmolVLM baseline for low-memory visual triage".into(),
            },
        ]
    }

    /// Resolve a model identifier or short alias (`None` resolves to `Qwen/Qwen2-VL-2B-Instruct`).
    pub fn resolve_catalog_entry(model_or_alias: Option<&str>) -> Result<ModelCatalogEntry> {
        let catalog = Self::catalog();
        let Some(query_raw) = model_or_alias.map(str::trim).filter(|s| !s.is_empty()) else {
            return Ok(catalog.into_iter().next().unwrap());
        };

        let lower = query_raw.to_ascii_lowercase();
        for entry in catalog {
            if entry.alias.eq_ignore_ascii_case(query_raw)
                || entry.model_id.eq_ignore_ascii_case(query_raw)
                || (lower == "qwen2-vl" && entry.alias == "qwen2-vl-2b")
                || (lower == "smolvlm2" && entry.alias == "smolvlm2-2.2b")
            {
                return Ok(entry);
            }
        }

        if query_raw.contains('/') {
            let arch = if lower.contains("qwen") {
                VlmModelArchitecture::Qwen2Vl
            } else {
                VlmModelArchitecture::SmolVlm
            };
            let alias = query_raw
                .rsplit('/')
                .next()
                .unwrap_or(query_raw)
                .to_ascii_lowercase();
            return Ok(ModelCatalogEntry {
                model_id: query_raw.to_string(),
                alias,
                architecture: arch,
                approx_size_gb: 4.0,
                is_default: false,
                description: "Custom HuggingFace Hub VLM repository".into(),
            });
        }

        Err(GmError::Config(format!(
            "Unknown VLM model '{query_raw}'. Supported aliases: qwen2-vl-2b (default), smolvlm2-2.2b, smolvlm, or <org>/<repo>"
        )))
    }

    pub fn model_dir_for(&self, model_id: &str) -> PathBuf {
        let sanitized: String = model_id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        self.models_dir.join(sanitized)
    }

    /// List all catalog models and their local installation status.
    pub fn list_models(&self) -> Vec<LocalModelStatus> {
        Self::catalog()
            .into_iter()
            .map(|entry| {
                let dir = self.model_dir_for(&entry.model_id);
                let (installed, has_weights, has_tokenizer, size_bytes) =
                    Self::inspect_model_dir(&dir);
                LocalModelStatus {
                    model_id: entry.model_id,
                    alias: entry.alias,
                    architecture: entry.architecture,
                    is_default: entry.is_default,
                    installed,
                    has_weights,
                    has_tokenizer,
                    size_bytes,
                    install_dir: dir,
                    description: entry.description,
                }
            })
            .collect()
    }

    fn inspect_model_dir(dir: &Path) -> (bool, bool, bool, u64) {
        if !dir.exists() {
            return (false, false, false, 0);
        }
        let mut total_bytes = 0u64;
        let mut has_weights = false;
        let mut has_tokenizer = dir.join("tokenizer.json").exists();

        if let Ok(entries) = fs::read_dir(dir) {
            for ent in entries.flatten() {
                let p = ent.path();
                if let Ok(meta) = ent.metadata()
                    && meta.is_file()
                {
                    total_bytes += meta.len();
                    if let Some(name) = p.file_name().and_then(|s| s.to_str()) {
                        if name.ends_with(".safetensors") {
                            has_weights = true;
                        }
                        if name == "tokenizer.json" {
                            has_tokenizer = true;
                        }
                    }
                }
            }
        }

        let installed = has_weights && (has_tokenizer || dir.join("manifest.json").exists());
        (installed, has_weights, has_tokenizer, total_bytes)
    }

    /// Pull a model's `config.json`, `tokenizer.json`, and `.safetensors` weights from HuggingFace Hub.
    pub async fn pull_model(
        &self,
        model_or_alias: Option<&str>,
        base_url_override: Option<&str>,
    ) -> Result<InstalledModelManifest> {
        let entry = Self::resolve_catalog_entry(model_or_alias)?;
        let target_dir = self.model_dir_for(&entry.model_id);
        fs::create_dir_all(&target_dir)?;

        let base_url = base_url_override
            .map(|s| s.trim_end_matches('/').to_string())
            .or_else(|| std::env::var("GM_HF_ENDPOINT").ok())
            .unwrap_or_else(|| DEFAULT_HF_BASE_URL.to_string());

        let hf_token = std::env::var("HF_TOKEN")
            .or_else(|_| std::env::var("HUGGING_FACE_HUB_TOKEN"))
            .ok()
            .filter(|t| !t.trim().is_empty());

        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(15))
            .build()
            .map_err(|e| {
                GmError::Provider(format!("Failed to build HTTP client for HF Hub: {e}"))
            })?;

        let mut downloaded_files = Vec::new();

        // 1. Download config.json (optional on custom repos, required on HF standard repos)
        if let Ok(file_meta) = Self::download_repo_file(
            &client,
            &base_url,
            &entry.model_id,
            "config.json",
            &target_dir,
            hf_token.as_deref(),
        )
        .await
        {
            downloaded_files.push(file_meta);
        }

        // 2. Download tokenizer.json
        let tok_meta = Self::download_repo_file(
            &client,
            &base_url,
            &entry.model_id,
            "tokenizer.json",
            &target_dir,
            hf_token.as_deref(),
        )
        .await?;
        downloaded_files.push(tok_meta);

        // 3. Download single-file `model.safetensors` or sharded `model.safetensors.index.json`
        match Self::download_repo_file(
            &client,
            &base_url,
            &entry.model_id,
            "model.safetensors",
            &target_dir,
            hf_token.as_deref(),
        )
        .await
        {
            Ok(weights_meta) => {
                downloaded_files.push(weights_meta);
            }
            Err(_) => {
                let index_meta = Self::download_repo_file(
                    &client,
                    &base_url,
                    &entry.model_id,
                    "model.safetensors.index.json",
                    &target_dir,
                    hf_token.as_deref(),
                )
                .await
                .map_err(|e| {
                    GmError::Provider(format!(
                        "Neither model.safetensors nor model.safetensors.index.json could be pulled for {}: {e}",
                        entry.model_id
                    ))
                })?;
                downloaded_files.push(index_meta);

                let index_raw =
                    fs::read_to_string(target_dir.join("model.safetensors.index.json"))?;
                let index_json: serde_json::Value = serde_json::from_str(&index_raw)?;
                let mut shards = BTreeSet::new();
                if let Some(weight_map) = index_json.get("weight_map").and_then(|w| w.as_object()) {
                    for val in weight_map.values() {
                        if let Some(shard_name) = val.as_str() {
                            shards.insert(shard_name.to_string());
                        }
                    }
                }
                if shards.is_empty() {
                    return Err(GmError::Provider(format!(
                        "model.safetensors.index.json for {} listed zero weight shards",
                        entry.model_id
                    )));
                }
                for shard in shards {
                    let shard_meta = Self::download_repo_file(
                        &client,
                        &base_url,
                        &entry.model_id,
                        &shard,
                        &target_dir,
                        hf_token.as_deref(),
                    )
                    .await?;
                    downloaded_files.push(shard_meta);
                }
            }
        }

        let total_bytes: u64 = downloaded_files.iter().map(|f| f.size_bytes).sum();
        let manifest = InstalledModelManifest {
            model_id: entry.model_id.clone(),
            alias: entry.alias.clone(),
            architecture: entry.architecture,
            pulled_at: chrono::Utc::now().to_rfc3339(),
            files: downloaded_files,
            total_bytes,
        };

        let manifest_path = target_dir.join("manifest.json");
        fs::write(&manifest_path, serde_json::to_string_pretty(&manifest)?)?;

        info!(
            "Pulled local VLM {} ({} bytes) into {:?}",
            entry.model_id, total_bytes, target_dir
        );
        Ok(manifest)
    }

    async fn download_repo_file(
        client: &reqwest::Client,
        base_url: &str,
        repo_id: &str,
        filename: &str,
        target_dir: &Path,
        hf_token: Option<&str>,
    ) -> Result<DownloadedModelFile> {
        let url = format!("{base_url}/{repo_id}/resolve/main/{filename}");
        let mut req = client.get(&url);
        if let Some(token) = hf_token {
            req = req.bearer_auth(token);
        }

        let resp = req
            .send()
            .await
            .map_err(|e| GmError::Provider(format!("GET {url} failed: {e}")))?;

        if !resp.status().is_success() {
            return Err(GmError::Provider(format!(
                "GET {url} returned HTTP {}",
                resp.status()
            )));
        }

        let dest_path = target_dir.join(filename);
        if let Some(parent) = dest_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let part_path = target_dir.join(format!("{filename}.part"));
        let mut out_file = fs::File::create(&part_path)?;
        let mut stream = resp.bytes_stream();
        let mut size_bytes = 0u64;
        let mut all_for_hash = Vec::new();

        while let Some(chunk_res) = stream.next().await {
            let chunk = chunk_res.map_err(|e| {
                GmError::Provider(format!("Stream error downloading {filename}: {e}"))
            })?;
            out_file.write_all(&chunk)?;
            size_bytes += chunk.len() as u64;
            if all_for_hash.len() < 64 * 1024 * 1024 {
                all_for_hash.extend_from_slice(&chunk);
            }
        }
        out_file.flush()?;
        drop(out_file);
        fs::rename(&part_path, &dest_path)?;

        let sha256 = compute_sha256(&all_for_hash);
        Ok(DownloadedModelFile {
            filename: filename.to_string(),
            size_bytes,
            sha256,
        })
    }

    /// Remove an installed local VLM directory.
    pub fn remove_model(&self, model_or_alias: &str) -> Result<PathBuf> {
        let entry = Self::resolve_catalog_entry(Some(model_or_alias))?;
        let dir = self.model_dir_for(&entry.model_id);
        if !dir.exists() {
            return Err(GmError::Config(format!(
                "Model '{}' is not installed at {:?}",
                entry.model_id, dir
            )));
        }
        fs::remove_dir_all(&dir)?;
        Ok(dir)
    }

    /// Instantiate a `CandleVlmEngine` from an installed model directory.
    pub fn load_engine(&self, model_or_alias: Option<&str>) -> Result<CandleVlmEngine> {
        let entry = Self::resolve_catalog_entry(model_or_alias)?;
        let dir = self.model_dir_for(&entry.model_id);
        if !dir.exists() {
            return Err(GmError::Config(format!(
                "Local VLM '{}' not found at {:?}. Run `gm models pull {}` first.",
                entry.model_id, dir, entry.alias
            )));
        }

        let mut cfg =
            CandleVlmConfig::from_model_id(&entry.model_id).unwrap_or_else(|| {
                match entry.architecture {
                    VlmModelArchitecture::Qwen2Vl => CandleVlmConfig::qwen2_vl_preset(),
                    VlmModelArchitecture::SmolVlm => CandleVlmConfig::smolvlm2_preset(),
                }
            });
        cfg.model_id = entry.model_id.clone();

        let tok_path = dir.join("tokenizer.json");
        if tok_path.exists() {
            cfg = cfg.with_tokenizer_path(tok_path);
        }

        let single_weights = dir.join("model.safetensors");
        if single_weights.exists() {
            cfg = cfg.with_weights_path(single_weights);
        } else {
            let mut shards: Vec<PathBuf> = fs::read_dir(&dir)?
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .map(|n| n.ends_with(".safetensors"))
                        .unwrap_or(false)
                })
                .collect();
            shards.sort();
            if shards.is_empty() {
                return Err(GmError::Platform(format!(
                    "No .safetensors files found in {:?}",
                    dir
                )));
            }
            cfg = cfg.with_shard_paths(shards);
        }

        CandleVlmEngine::new(cfg)
    }

    /// Return the highest-priority installed local VLM engine, if any is pulled on disk.
    pub fn find_installed_engine(&self) -> Option<CandleVlmEngine> {
        for status in self.list_models() {
            if status.installed
                && let Ok(engine) = self.load_engine(Some(&status.model_id))
            {
                return Some(engine);
            }
        }
        None
    }
}

/// Format a human-readable CLI table for `gm models list`.
pub fn format_models_table(models: &[LocalModelStatus]) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{:<16} {:<36} {:<10} {:<12} {}\n",
        "ALIAS", "MODEL ID", "ARCH", "STATUS", "SIZE"
    ));
    out.push_str(&format!("{}\n", "-".repeat(88)));
    for m in models {
        let default_tag = if m.is_default { " *" } else { "" };
        let alias_col = format!("{}{}", m.alias, default_tag);
        let status_col = if m.installed {
            "installed"
        } else {
            "available"
        };
        let size_col = if m.size_bytes > 0 {
            format!("{:.2} MB", (m.size_bytes as f64) / (1024.0 * 1024.0))
        } else {
            "-".to_string()
        };
        out.push_str(&format!(
            "{:<16} {:<36} {:<10} {:<12} {}\n",
            alias_col, m.model_id, m.architecture, status_col, size_col
        ));
    }
    out.push_str("\n(* = default for `gm models pull`)\n");
    out
}
