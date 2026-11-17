use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};
use walkdir::WalkDir;

const CHUNK_SIZE: usize = 900;
const CHUNK_OVERLAP: usize = 150;
const EXTENSIONS: [&str; 5] = ["txt", "md", "markdown", "csv", "json"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chunk {
    pub source: String,
    pub text: String,
    #[serde(default)]
    pub embedding: Option<Vec<f32>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Index {
    #[serde(default)]
    pub chunks: Vec<Chunk>,
}

fn index_path(app: &AppHandle) -> Result<PathBuf> {
    Ok(app.path().app_config_dir()?.join("rag_index.json"))
}

pub fn load(app: &AppHandle) -> Index {
    index_path(app)
        .ok()
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save(app: &AppHandle, index: &Index) -> Result<()> {
    let path = index_path(app)?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(&path, serde_json::to_string_pretty(index)?).context("writing rag_index.json")
}

pub fn clear(app: &AppHandle) -> Result<()> {
    let path = index_path(app)?;
    if path.exists() {
        fs::remove_file(path)?;
    }
    Ok(())
}

/// Split text into roughly `size`-character chunks with `overlap` characters
/// of shared context. Operates on chars so multi-byte text never splits a
/// codepoint.
fn chunk_text(text: &str, size: usize, overlap: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= size {
        let trimmed = text.trim();
        return if trimmed.is_empty() {
            Vec::new()
        } else {
            vec![trimmed.to_string()]
        };
    }

    let step = size.saturating_sub(overlap).max(1);
    let mut out = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        let end = (start + size).min(chars.len());
        let piece: String = chars[start..end].iter().collect();
        let trimmed = piece.trim();
        if !trimmed.is_empty() {
            out.push(trimmed.to_string());
        }
        if end == chars.len() {
            break;
        }
        start += step;
    }
    out
}

/// Walk a folder and turn every supported text file into chunks.
pub fn read_folder(path: &str) -> Result<Vec<Chunk>> {
    let root = PathBuf::from(path.trim());
    if !root.is_dir() {
        anyhow::bail!("not a folder: {}", root.display());
    }

    let mut chunks = Vec::new();
    for entry in WalkDir::new(&root).follow_links(false).into_iter().flatten() {
        if !entry.file_type().is_file() {
            continue;
        }
        let file = entry.path();
        let extension = file
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !EXTENSIONS.contains(&extension.as_str()) {
            continue;
        }
        let Ok(text) = fs::read_to_string(file) else {
            continue;
        };
        for piece in chunk_text(&text, CHUNK_SIZE, CHUNK_OVERLAP) {
            chunks.push(Chunk {
                source: file.display().to_string(),
                text: piece,
                embedding: None,
            });
        }
    }
    Ok(chunks)
}

fn tokens(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_lowercase())
        .collect()
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let mut dot = 0.0;
    let mut na = 0.0;
    let mut nb = 0.0;
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na.sqrt() * nb.sqrt())
    }
}

fn bm25(index: &Index, query: &str) -> Vec<(f64, usize)> {
    const K1: f64 = 1.5;
    const B: f64 = 0.75;

    let docs: Vec<Vec<String>> = index.chunks.iter().map(|c| tokens(&c.text)).collect();
    let total = docs.len();
    if total == 0 {
        return Vec::new();
    }

    let avg_len = docs.iter().map(|d| d.len() as f64).sum::<f64>() / total as f64;
    let query_tokens: HashSet<String> = tokens(query).into_iter().collect();
    if query_tokens.is_empty() {
        return Vec::new();
    }

    let mut doc_freq: HashMap<&str, f64> = HashMap::new();
    for token in &query_tokens {
        let count = docs.iter().filter(|d| d.contains(token)).count() as f64;
        doc_freq.insert(token.as_str(), count);
    }

    docs.iter()
        .enumerate()
        .map(|(i, doc)| {
            let mut score = 0.0;
            for token in &query_tokens {
                let tf = doc.iter().filter(|t| *t == token).count() as f64;
                if tf == 0.0 {
                    continue;
                }
                let df = *doc_freq.get(token.as_str()).unwrap_or(&0.0);
                let idf = ((total as f64 - df + 0.5) / (df + 0.5) + 1.0).ln();
                let norm = 1.0 - B + B * doc.len() as f64 / avg_len.max(1.0);
                score += idf * tf * (K1 + 1.0) / (tf + K1 * norm);
            }
            (score, i)
        })
        .collect()
}

/// Return the top `k` chunks for `query`. Uses cosine similarity when a query
/// embedding is available, otherwise BM25.
pub fn retrieve(
    index: &Index,
    query: &str,
    k: usize,
    query_embedding: Option<&[f32]>,
) -> Vec<Chunk> {
    if index.chunks.is_empty() || k == 0 {
        return Vec::new();
    }

    let has_embeddings = index.chunks.iter().any(|c| c.embedding.is_some());

    if let (Some(query_vec), true) = (query_embedding, has_embeddings) {
        let mut scored: Vec<(f32, usize)> = index
            .chunks
            .iter()
            .enumerate()
            .filter_map(|(i, chunk)| {
                let embedding = chunk.embedding.as_ref()?;
                if embedding.len() != query_vec.len() {
                    return None;
                }
                Some((cosine(query_vec, embedding), i))
            })
            .collect();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(Ordering::Equal));
        return scored
            .into_iter()
            .take(k)
            .map(|(_, i)| index.chunks[i].clone())
            .collect();
    }

    let mut scored = bm25(index, query);
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(Ordering::Equal));
    scored
        .into_iter()
        .filter(|(score, _)| *score > 0.0)
        .take(k)
        .map(|(_, i)| index.chunks[i].clone())
        .collect()
}
