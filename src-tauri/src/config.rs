use std::{fs, path::PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

pub const DEFAULT_SYSTEM_PROMPT: &str = "You answer questions for the user. Prefer the provided context when it clearly answers the question; otherwise use your own general knowledge. Output only the final answer, as short as possible. No explanations, no quotes, no markdown.";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub provider: String,
    pub api_key: String,
    pub base_url: String,
    pub vision_model: String,
    pub answer_model: String,
    pub embedding_model: String,
    pub system_prompt: String,
    pub top_k: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            provider: "opencode".into(),
            api_key: String::new(),
            base_url: String::new(),
            vision_model: String::new(),
            answer_model: String::new(),
            embedding_model: String::new(),
            system_prompt: DEFAULT_SYSTEM_PROMPT.into(),
            top_k: 4,
        }
    }
}

impl Config {
    fn path(app: &AppHandle) -> Result<PathBuf> {
        Ok(app.path().app_config_dir()?.join("config.json"))
    }

    pub fn load(app: &AppHandle) -> Self {
        Self::path(app)
            .ok()
            .and_then(|p| fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(s.trim_start_matches('\u{feff}')).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, app: &AppHandle) -> Result<()> {
        let path = Self::path(app)?;
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(&path, serde_json::to_string_pretty(self)?).context("writing config.json")
    }

    pub fn is_anthropic(&self) -> bool {
        self.provider.eq_ignore_ascii_case("anthropic")
    }

    pub fn is_gemini(&self) -> bool {
        self.provider.eq_ignore_ascii_case("gemini")
    }

    pub fn base_url(&self) -> String {
        let trimmed = self.base_url.trim().trim_end_matches('/');
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
        match self.provider.to_ascii_lowercase().as_str() {
            "anthropic" => "https://api.anthropic.com",
            "openai" => "https://api.openai.com/v1",
            "gemini" => "https://generativelanguage.googleapis.com/v1beta/openai",
            "openrouter" => "https://openrouter.ai/api/v1",
            _ => "https://opencode.ai/zen/v1",
        }
        .to_string()
    }

    pub fn embeddings_enabled(&self) -> bool {
        !self.embedding_model.trim().is_empty()
    }
}
