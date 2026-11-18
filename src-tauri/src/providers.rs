use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

use crate::config::Config;

fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(20))
            .timeout(Duration::from_secs(150))
            .build()
            .expect("failed to build HTTP client")
    })
}

fn snippet(body: &str) -> String {
    let body = body.trim();
    if body.chars().count() > 800 {
        let short: String = body.chars().take(800).collect();
        format!("{short}...")
    } else {
        body.to_string()
    }
}

async fn send_error(provider: &str, response: reqwest::Response) -> anyhow::Error {
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if status.as_u16() == 429 && body.contains("FreeUsageLimitError") {
        return anyhow!(
            "Free limit reached on this key — free models are capped per account. \
             Wait for the cap to reset, switch to another provider, or subscribe to OpenCode Go."
        );
    }
    if status.as_u16() == 429 {
        return anyhow!(
            "{provider}: quota reached for this key. Wait for it to reset, or switch provider or model."
        );
    }
    anyhow!(
        "{provider} returned HTTP {}: {}",
        status.as_u16(),
        provider_message(&body)
    )
}

/// Pull the human sentence out of a provider error body instead of showing
/// the raw JSON. Gemini wraps the error in an array; OpenAI-style uses an object.
fn provider_message(body: &str) -> String {
    if let Ok(value) = serde_json::from_str::<Value>(body) {
        let entry = value.get(0).unwrap_or(&value);
        if let Some(message) = entry
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
        {
            let first = message.split(". ").next().unwrap_or(message);
            return first.chars().take(160).collect();
        }
    }
    snippet(body)
}

fn json_error(text: &str) -> anyhow::Error {
    anyhow!("could not read provider response: {}", snippet(text))
}

pub async fn chat(
    cfg: &Config,
    system: &str,
    user: &str,
    image_png_b64: Option<&str>,
    model: &str,
) -> Result<String> {
    if model.trim().is_empty() {
        bail!("no model configured");
    }
    if cfg.api_key.trim().is_empty() {
        bail!("no API key configured");
    }

    if cfg.is_anthropic() {
        anthropic_chat(cfg, system, user, image_png_b64, model.trim()).await
    } else {
        openai_chat(cfg, system, user, image_png_b64, model.trim()).await
    }
}

async fn openai_chat(
    cfg: &Config,
    system: &str,
    user: &str,
    image_png_b64: Option<&str>,
    model: &str,
) -> Result<String> {
    let url = format!("{}/chat/completions", cfg.base_url());

    let content = match image_png_b64 {
        Some(image) => json!([
            { "type": "text", "text": user },
            {
                "type": "image_url",
                "image_url": { "url": format!("data:image/png;base64,{image}") }
            }
        ]),
        None => json!(user),
    };

    let mut messages = Vec::new();
    if !system.trim().is_empty() {
        messages.push(json!({ "role": "system", "content": system }));
    }
    messages.push(json!({ "role": "user", "content": content }));

    let response = client()
        .post(&url)
        .bearer_auth(cfg.api_key.trim())
        .json(&json!({ "model": model, "messages": messages }))
        .send()
        .await
        .with_context(|| format!("request to {url} failed"))?;

    if !response.status().is_success() {
        return Err(send_error("the provider", response).await);
    }

    let text = response.text().await?;
    let value: Value = serde_json::from_str(&text).map_err(|_| json_error(&text))?;
    let content = &value["choices"][0]["message"]["content"];

    let answer = if let Some(s) = content.as_str() {
        s.to_string()
    } else if let Some(parts) = content.as_array() {
        parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("")
    } else {
        String::new()
    };

    if answer.trim().is_empty() {
        bail!("the provider returned an empty answer");
    }
    Ok(answer)
}

async fn anthropic_chat(
    cfg: &Config,
    system: &str,
    user: &str,
    image_png_b64: Option<&str>,
    model: &str,
) -> Result<String> {
    let url = format!("{}/v1/messages", cfg.base_url());

    let mut content = Vec::new();
    if let Some(image) = image_png_b64 {
        content.push(json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": "image/png",
                "data": image
            }
        }));
    }
    content.push(json!({ "type": "text", "text": user }));

    let response = client()
        .post(&url)
        .header("x-api-key", cfg.api_key.trim())
        .header("anthropic-version", "2023-06-01")
        .json(&json!({
            "model": model,
            "max_tokens": 1024,
            "system": system,
            "messages": [{ "role": "user", "content": content }]
        }))
        .send()
        .await
        .with_context(|| format!("request to {url} failed"))?;

    if !response.status().is_success() {
        return Err(send_error("Anthropic", response).await);
    }

    let text = response.text().await?;
    let value: Value = serde_json::from_str(&text).map_err(|_| json_error(&text))?;
    let answer = value["content"]
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b["type"] == "text")
                .filter_map(|b| b["text"].as_str())
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default();

    if answer.trim().is_empty() {
        bail!("Anthropic returned an empty answer");
    }
    Ok(answer)
}

/// Streaming answer: calls `on_delta` with each text fragment as it arrives.
/// Returns the full text once the stream ends.
pub async fn chat_stream<F>(
    cfg: &Config,
    system: &str,
    user: &str,
    model: &str,
    mut on_delta: F,
) -> Result<String>
where
    F: FnMut(&str),
{
    if model.trim().is_empty() {
        bail!("no model configured");
    }
    if cfg.api_key.trim().is_empty() {
        bail!("no API key configured");
    }

    let (url, request) = if cfg.is_anthropic() {
        (
            format!("{}/v1/messages", cfg.base_url()),
            client()
                .post(format!("{}/v1/messages", cfg.base_url()))
                .header("x-api-key", cfg.api_key.trim())
                .header("anthropic-version", "2023-06-01")
                .json(&json!({
                    "model": model.trim(),
                    "max_tokens": 1024,
                    "system": system,
                    "stream": true,
                    "messages": [{ "role": "user", "content": user }]
                })),
        )
    } else {
        let mut messages = Vec::new();
        if !system.trim().is_empty() {
            messages.push(json!({ "role": "system", "content": system }));
        }
        messages.push(json!({ "role": "user", "content": user }));
        (
            format!("{}/chat/completions", cfg.base_url()),
            client()
                .post(format!("{}/chat/completions", cfg.base_url()))
                .bearer_auth(cfg.api_key.trim())
                .json(&json!({ "model": model.trim(), "messages": messages, "stream": true })),
        )
    };

    let mut response = request
        .send()
        .await
        .with_context(|| format!("request to {url} failed"))?;

    if !response.status().is_success() {
        return Err(send_error("the provider", response).await);
    }

    let is_sse = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.contains("event-stream"))
        .unwrap_or(false);

    // Some providers ignore `stream` and answer with one JSON body.
    if !is_sse {
        let text = response.text().await?;
        let value: Value = serde_json::from_str(&text).map_err(|_| json_error(&text))?;
        let answer = if cfg.is_anthropic() {
            anthropic_answer(&value)
        } else {
            openai_answer(&value)
        };
        if answer.trim().is_empty() {
            bail!("the provider returned an empty answer");
        }
        on_delta(&answer);
        return Ok(answer);
    }

    let mut answer = String::new();
    let mut buf: Vec<u8> = Vec::new();
    let mut done = false;

    while !done {
        let Some(chunk) = response.chunk().await? else {
            break;
        };
        buf.extend_from_slice(&chunk);

        while let Some(pos) = buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end_matches(['\r', '\n']);
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            if data == "[DONE]" {
                done = true;
                break;
            }
            if data.is_empty() {
                continue;
            }
            let Ok(value) = serde_json::from_str::<Value>(data) else {
                continue; // keep-alive or a comment line
            };

            if cfg.is_anthropic() {
                match value["type"].as_str() {
                    Some("content_block_delta") => {
                        if let Some(text) = value["delta"]["text"].as_str() {
                            if !text.is_empty() {
                                answer.push_str(text);
                                on_delta(text);
                            }
                        }
                    }
                    Some("error") => {
                        let message = value["error"]["message"].as_str().unwrap_or("stream error");
                        bail!("Anthropic: {message}");
                    }
                    _ => {}
                }
            } else {
                let content = &value["choices"][0]["delta"]["content"];
                let text = if let Some(s) = content.as_str() {
                    s.to_string()
                } else if let Some(parts) = content.as_array() {
                    parts
                        .iter()
                        .filter_map(|p| p["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("")
                } else {
                    String::new()
                };
                if !text.is_empty() {
                    answer.push_str(&text);
                    on_delta(&text);
                }
            }
        }
    }

    if answer.trim().is_empty() {
        bail!("the provider returned an empty answer");
    }
    Ok(answer)
}

/// Text out of an OpenAI-style non-streaming completion body.
fn openai_answer(value: &Value) -> String {
    let content = &value["choices"][0]["message"]["content"];
    if let Some(s) = content.as_str() {
        s.to_string()
    } else if let Some(parts) = content.as_array() {
        parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("")
    } else {
        String::new()
    }
}

/// Text out of an Anthropic non-streaming messages body.
fn anthropic_answer(value: &Value) -> String {
    value["content"]
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b["type"] == "text")
                .filter_map(|b| b["text"].as_str())
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

pub async fn embed(cfg: &Config, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
    if cfg.is_anthropic() {
        bail!("Anthropic has no embeddings endpoint. Leave the embedding model empty to use keyword search.");
    }

    let url = format!("{}/embeddings", cfg.base_url());
    let response = client()
        .post(&url)
        .bearer_auth(cfg.api_key.trim())
        .json(&json!({ "model": cfg.embedding_model.trim(), "input": inputs }))
        .send()
        .await
        .with_context(|| format!("request to {url} failed"))?;

    if !response.status().is_success() {
        return Err(send_error("the embeddings endpoint", response).await);
    }

    let text = response.text().await?;
    let value: Value = serde_json::from_str(&text).map_err(|_| json_error(&text))?;

    let vectors: Vec<Vec<f32>> = value["data"]
        .as_array()
        .ok_or_else(|| json_error(&text))?
        .iter()
        .map(|item| {
            item["embedding"]
                .as_array()
                .map(|values| {
                    values
                        .iter()
                        .map(|v| v.as_f64().unwrap_or(0.0) as f32)
                        .collect()
                })
                .unwrap_or_default()
        })
        .collect();

    if vectors.len() != inputs.len() {
        bail!(
            "embeddings endpoint returned {} vectors for {} inputs",
            vectors.len(),
            inputs.len()
        );
    }
    Ok(vectors)
}

pub async fn list_models(cfg: &Config) -> Result<Vec<String>> {
    if cfg.api_key.trim().is_empty() {
        bail!("set your API key first");
    }

    if cfg.is_gemini() {
        return gemini_models(cfg).await;
    }

    let base = cfg.base_url();
    let request = if cfg.is_anthropic() {
        client()
            .get(format!("{base}/v1/models?limit=1000"))
            .header("x-api-key", cfg.api_key.trim())
            .header("anthropic-version", "2023-06-01")
    } else {
        client()
            .get(format!("{base}/models"))
            .bearer_auth(cfg.api_key.trim())
    };

    let response = request.send().await.context("could not reach provider")?;
    if !response.status().is_success() {
        return Err(send_error("the provider", response).await);
    }

    let text = response.text().await?;
    let value: Value = serde_json::from_str(&text).map_err(|_| json_error(&text))?;

    let mut models: Vec<String> = value["data"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item["id"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    models.sort();
    models.dedup();

    if models.is_empty() {
        bail!("the provider returned no models");
    }
    Ok(models)
}

/// Gemini's OpenAI-compatible layer has no model list, so ask the native endpoint.
async fn gemini_models(cfg: &Config) -> Result<Vec<String>> {
    let url = "https://generativelanguage.googleapis.com/v1beta/models?pageSize=1000";
    let response = client()
        .get(url)
        .header("x-goog-api-key", cfg.api_key.trim())
        .send()
        .await
        .context("could not reach Gemini")?;

    if !response.status().is_success() {
        return Err(send_error("Gemini", response).await);
    }

    let text = response.text().await?;
    let value: Value = serde_json::from_str(&text).map_err(|_| json_error(&text))?;

    let mut models: Vec<String> = value["models"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter(|item| {
                    item["supportedGenerationMethods"]
                        .as_array()
                        .map(|methods| methods.iter().any(|m| m == "generateContent"))
                        .unwrap_or(true)
                })
                .filter_map(|item| item["name"].as_str())
                .map(|name| name.trim_start_matches("models/").to_string())
                .collect()
        })
        .unwrap_or_default();

    models.sort();
    models.dedup();

    if models.is_empty() {
        bail!("Gemini returned no chat models for this key");
    }
    Ok(models)
}
