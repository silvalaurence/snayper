# SnipBots: Architecture & Build Spec

A Windows desktop app (Rust + Tauri v2). The user presses a hotkey, drags a rectangle over any question on screen (assignment, web form, quiz page), and two animated "bots" fly across the screen:

1. **Snip Bot** captures the selected region and reads the text from it.
2. **Answer Bot** answers the question, using the user's own knowledge base (RAG) plus the user's own AI API key.

The answer appears in a small bubble next to the question. **The user types the answer themselves**; the app never clicks or types into other windows.

Example: the screen shows "1. What is the green planet?" -> Snip Bot reads it -> Answer Bot replies "Earth" -> user types "Earth".

---

## 1. Tech stack

| Layer | Choice |
|---|---|
| App shell | Tauri v2 (Rust backend, WebView frontend) |
| Language | Rust (backend), plain HTML/CSS/JS (frontend, no framework) |
| Screen capture | `xcap` crate |
| Image handling | `image` crate (crop + PNG encode), `base64` |
| HTTP | `reqwest` (rustls, json) |
| Async runtime | `tokio` (the one Tauri already uses) |
| Hotkeys | `tauri-plugin-global-shortcut` v2 |
| RAG storage | JSON file on disk (upgrade path: SQLite) |
| File walking | `walkdir` |
| Errors | `anyhow` |
| Settings storage | JSON in the app config dir |

Target OS: **Windows 10/11** only for now.

---

## 2. Project layout

```
snipbots/
├── ARCHITECTURE.md
├── README.md
├── ui/                          # frontend (served by Tauri, no bundler)
│   ├── index.html               # settings window
│   ├── settings.js
│   ├── overlay.html             # transparent fullscreen overlay
│   ├── overlay.js               # snip selection + flying bots + answer bubble
│   └── overlay.css
└── src-tauri/
    ├── Cargo.toml
    ├── build.rs
    ├── tauri.conf.json
    ├── capabilities/default.json
    ├── icons/icon.ico           # generate with `cargo tauri icon logo.png`
    └── src/
        ├── main.rs              # app setup, hotkeys, windows, Tauri commands, pipeline
        ├── config.rs            # Config struct, load/save, default base URLs
        ├── providers.rs         # chat(), embed(), list_models() for each provider
        ├── rag.rs               # chunking, ingest, embeddings, BM25, retrieve
        └── capture.rs           # (optional split) screen capture + crop + PNG encode
```

---

## 3. Windows (Tauri)

Two windows are declared in `tauri.conf.json`:

**`main`**: normal window, the settings panel. Visible at launch.

**`overlay`**: transparent, always on top, no decorations, no shadow, `skipTaskbar`, **hidden by default**.
- Covers the entire primary monitor (Rust sets position and size from `primary_monitor()` each time before showing).
- Two modes, switched from Rust with `set_ignore_cursor_events`:
  - **Selection mode** (`false`): captures mouse so the user can drag a rectangle.
  - **Click-through mode** (`true`): mouse passes to the apps underneath, so the user can type in the form while bots and the answer bubble remain visible.

`withGlobalTauri: true` so the frontend uses `window.__TAURI__.core.invoke` and `window.__TAURI__.event.listen` without a bundler.

---

## 4. Hotkeys (registered in Rust)

| Hotkey | Action |
|---|---|
| `Ctrl+Shift+S` | Start a snip |
| `Ctrl+Shift+X` | Hide the overlay and clear the bots and bubble |

---

## 5. Data models

```rust
// config.rs
struct Config {
    provider: String,        // "opencode" | "openai" | "anthropic"
    api_key: String,
    base_url: String,        // empty = provider default
    vision_model: String,    // snip bot, must accept images
    answer_model: String,    // answer bot
    embedding_model: String, // empty = RAG uses keyword search instead of vectors
    system_prompt: String,   // answer style
    top_k: usize,            // RAG chunks to retrieve (default 4)
}

// rag.rs
struct Chunk { source: String, text: String, embedding: Option<Vec<f32>> }
struct Index { chunks: Vec<Chunk> }

// main.rs
struct AppState {
    cfg: Mutex<Config>,
    shot: Mutex<Option<(u32, u32, Vec<u8>)>>, // width, height, raw RGBA of the full-screen capture
    index: Mutex<Index>,
}
struct Rect { x: f64, y: f64, w: f64, h: f64, dpr: f64 } // CSS px from the overlay + devicePixelRatio
struct Stage { stage: &'static str, text: String }       // event payload to the overlay
```

Files on disk (in Tauri's app config / data dir):
- `config.json`: settings (note: API key is plain text. Upgrade path: Windows Credential Manager via the `keyring` crate.)
- `rag_index.json`: the knowledge index

---

## 6. Tauri commands (frontend -> backend)

| Command | Args | Returns | Purpose |
|---|---|---|---|
| `get_config` | none | `Config` | Load settings into the UI |
| `save_config` | `Config` | `()` | Persist settings and update `AppState` |
| `list_models` | none | `Vec<String>` | `GET {base}/models`, so the user can pick model IDs |
| `ingest_folder` | `path: String` | `usize` (chunks) | Build or extend the RAG index from a folder |
| `rag_stats` | none | `usize` | Number of chunks currently indexed |
| `clear_index` | none | `()` | Wipe the knowledge base |
| `start_snip` | none | `()` | Same as the hotkey (button in settings) |
| `process_snip` | `Rect` | `()` | Run the full bot pipeline (see section 8) |
| `hide_overlay` | none | `()` | Hide overlay, emit `reset` |

## 7. Events (backend -> overlay, via `emit_to("overlay", ...)`)

| Event | Payload | Meaning |
|---|---|---|
| `snip-start` | none | Overlay is visible; show dim layer and crosshair |
| `stage` | `{stage, text}` | Pipeline progress; `stage` is one of `snip`, `read`, `answer`, `error` |
| `reset` | none | Clear all bots, selection, bubble |

---

## 8. End-to-end flow

```
User                Rust backend                         Overlay (JS)              AI provider
 │ Ctrl+Shift+S        │                                    │                         │
 ├────────────────────►│ hide overlay, wait ~150 ms         │                         │
 │                     │ capture primary monitor -> AppState.shot (BEFORE overlay shows,
 │                     │ so the dim layer is never in the screenshot)                 │
 │                     │ position+size overlay to monitor   │                         │
 │                     │ set_ignore_cursor_events(false)    │                         │
 │                     │ show + focus, emit snip-start ────►│ dim screen, crosshair   │
 │ drag a rectangle    │                                    │ draw selection box      │
 ├────────────────────────────────────────────────────────►│ mouseup                 │
 │                     │◄──────── invoke process_snip(Rect{x,y,w,h,dpr}) ─────────────│
 │                     │ set_ignore_cursor_events(true)     │ remove dim layer        │
 │                     │ crop AppState.shot (x*dpr, y*dpr, w*dpr, h*dpr) -> PNG -> base64
 │                     │ emit stage:"snip" ───────────────►│ SNIP BOT flies to selection, flash
 │                     │ chat(vision_model, image) ───────────────────────────────────►│
 │                     │◄──────────────────── extracted question text ─────────────────│
 │                     │ emit stage:"read" (text) ────────►│ snip bot flies away,
 │                     │                                    │ ANSWER BOT flies in, "thinking"
 │                     │ rag::retrieve(question) -> top_k chunks (see section 9)
 │                     │ chat(answer_model, system_prompt + context + question) ──────►│
 │                     │◄──────────────────────── answer ──────────────────────────────│
 │                     │ emit stage:"answer" (text) ──────►│ answer bubble next to selection
 │ reads "Earth",      │                                    │ (click-through, stays visible)
 │ types it in the form│                                    │
 │ Ctrl+Shift+X        │ hide overlay, emit reset ────────►│ clear
```

Rules:
- Any failure inside `process_snip` emits `stage:"error"` with a readable message (missing API key, HTTP status and body, empty selection under 5 px, etc.). The command itself returns `Ok(())` so the UI handles it through the event.
- Never hold a `std::sync::Mutex` guard across an `.await`. Clone what is needed first.
- Coordinates: the overlay reports CSS pixels and `window.devicePixelRatio`. Multiply by `dpr` to get physical pixels, then clamp to the screenshot bounds.
- The capture returned by `xcap` is converted to `(w, h, Vec<u8>)` right away, so the app is not tied to a specific `image` crate version.

---

## 9. RAG ("training" the AI on your own material)

The model is **not fine-tuned**. "Training" means building a knowledge base from the user's notes, textbook excerpts, lecture slides (as text), past answers and so on, and giving the most relevant pieces to the answer bot with every question.

### 9.1 Ingest (user picks a folder, clicks "Build knowledge base")

1. Walk the folder recursively with `walkdir`.
2. Read files with extensions `txt, md, markdown, csv, json` (extend later with `pdf` text extraction and `docx`).
3. Split each file into chunks of about **900 characters with 150 characters overlap**, trimmed, empty chunks dropped.
4. Each chunk becomes `Chunk { source: file path, text, embedding: None }`.
5. **If `embedding_model` is set**: send chunks in batches of 32 to `POST {base}/embeddings` and store the returned vectors in `embedding`.
6. Save the whole `Index` to `rag_index.json` and keep it in `AppState.index`.

### 9.2 Retrieve (runs on every snip, after the question text is extracted)

**Mode A, vector search** (if `embedding_model` is set and chunks have embeddings):
1. Embed the question text with the same embedding model.
2. Cosine similarity against every chunk (brute force is fine up to roughly 50k chunks).
3. Take the top `top_k`.

**Mode B, keyword search (default, needs no embeddings API)**: BM25
1. Lowercase, split on non-alphanumeric characters into tokens.
2. Compute document frequency, average chunk length, then BM25 with `k1 = 1.5`, `b = 0.75`.
3. Take the top `top_k` with score > 0.

If the index is empty or nothing scores above zero, the context is just empty and the answer bot answers from the model's own knowledge.

### 9.3 Prompt assembly for the Answer Bot

```
system: {config.system_prompt}
user:
  Context (from the user's notes, may be irrelevant):
  [1] ({source}) {chunk text}
  [2] ...

  Question:
  {text extracted by the snip bot}
```

Instruction inside the system prompt: prefer the context when it clearly answers the question; otherwise answer from general knowledge; output only the final answer, as short as possible.

### 9.4 Upgrade path

- Swap `rag_index.json` for SQLite (+ `sqlite-vec` for vector search).
- Hybrid search: combine BM25 and cosine scores (reciprocal rank fusion).
- Local embeddings through Ollama (`/api/embeddings`) so no embedding API key is needed.
- Add a "save this Q&A to the knowledge base" button so confirmed answers feed back into RAG.

---

## 10. AI providers (`providers.rs`)

Common function signatures:

```rust
async fn chat(cfg: &Config, system: &str, user: &str,
              image_png_b64: Option<&str>, model: &str) -> anyhow::Result<String>;
async fn embed(cfg: &Config, inputs: &[String]) -> anyhow::Result<Vec<Vec<f32>>>;
async fn list_models(cfg: &Config) -> anyhow::Result<Vec<String>>;
```

Always check the HTTP status and return the response body in the error if it is not 2xx.

### 10.1 `opencode` and `openai` (OpenAI-compatible Chat Completions)

- Default base URLs: OpenCode Zen `https://opencode.ai/zen/v1`; OpenAI `https://api.openai.com/v1`
- `POST {base}/chat/completions`, header `Authorization: Bearer {api_key}`
- Body:
  ```json
  { "model": "...",
    "messages": [
      {"role": "system", "content": "..."},
      {"role": "user", "content": [
        {"type": "text", "text": "..."},
        {"type": "image_url", "image_url": {"url": "data:image/png;base64,..."}}
      ]}
    ] }
  ```
  (plain string `content` when there is no image)
- Answer is at `choices[0].message.content`
- `GET {base}/models` returns `data[].id`
- Embeddings: `POST {base}/embeddings` with `{model, input:[...]}` returning `data[].embedding`
- **OpenCode Zen caveat:** only Zen models that use the OpenAI-compatible `/chat/completions` endpoint work with this path. Zen's GPT models use `/responses` and its Claude models use `/messages`. OpenCode's Go plan uses base URL `https://opencode.ai/zen/go/v1` with the same API key. Check the live model list and the OpenCode docs, and not every model accepts images, so the vision model must be one that does. Verify these details before relying on them, since they can change.

### 10.2 `anthropic`

- Default base URL `https://api.anthropic.com`
- `POST {base}/v1/messages`, headers `x-api-key: {api_key}`, `anthropic-version: 2023-06-01`
- Body:
  ```json
  { "model": "...", "max_tokens": 1024, "system": "...",
    "messages": [{"role": "user", "content": [
      {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "..."}},
      {"type": "text", "text": "..."}
    ]}] }
  ```
- Answer is at `content[0].text`
- Setting `base_url` to `https://opencode.ai/zen` lets this path reach Claude models through OpenCode Zen (per OpenCode's docs; verify)
- No embeddings endpoint, so with this provider RAG uses BM25 (or set a separate embedding provider later)

### 10.3 Prompts

**Snip Bot (vision)** system prompt:
> You are an OCR assistant. Transcribe all text in the image exactly, including question numbers and answer options. Output only the text, no commentary.

user text: `Extract the text.`

**Answer Bot**: see section 9.3.

---

## 11. Frontend behavior

### 11.1 Settings window (`index.html` + `settings.js`)
- Fields: provider dropdown, API key (password input), base URL (placeholder shows the default), vision model, answer model, embedding model (optional), system prompt textarea, top_k.
- Buttons: **Save**, **List models** (fills a datalist for the model inputs), **Test snip** (calls `start_snip`).
- RAG panel: folder path text input, **Build knowledge base**, **Clear**, and a line showing the chunk count from `rag_stats`.
- Shows the hotkeys and a status line for errors.

### 11.2 Overlay (`overlay.html` + `overlay.js` + `overlay.css`)
- Transparent body. Layers: `#dim` (semi-transparent black, crosshair cursor), `#sel` (selection rectangle), `#stage` (bots), `#bubble` (answer).
- On `snip-start`: show `#dim`, listen for `mousedown/mousemove/mouseup` to draw `#sel`. `Esc` calls `hide_overlay`. A selection smaller than 5x5 px is ignored.
- On `mouseup`: hide `#dim`, invoke `process_snip({x, y, w, h, dpr: window.devicePixelRatio})`.
- Bots are small inline-SVG robots (rounded head, antenna, two eyes) with a CSS bobbing animation. Movement uses CSS `left/top` transitions (about 0.9 s, cubic-bezier) from off-screen to a target.
  - On `stage:"snip"`: **Snip Bot** (blue, with a scissors emoji) spawns off-screen top-left, flies to the selection's top-left corner, the selection box flashes.
  - On `stage:"read"`: Snip Bot flies off-screen and is removed. **Answer Bot** (green) spawns off-screen bottom-right, flies next to the selection, shows animated "thinking..." dots in the bubble, with the extracted question in small grey text.
  - On `stage:"answer"`: bubble shows the answer in large text next to the selection. Answer Bot idles beside it.
  - On `stage:"error"`: bubble turns red with the message, bots fly away.
  - On `reset`: remove everything.
- The bubble must stay inside the screen bounds (flip it left or above the selection if it would overflow).

---

## 12. Error handling and edge cases

- Missing API key or model name: emit an `error` stage with "Set your API key and models in SnipBots settings".
- Capture failure (permissions, no monitor): emit `error`.
- Multi-monitor: v1 captures the **primary** monitor only. Document this.
- High-DPI scaling: handled through `dpr` (section 8).
- Network timeout: `reqwest` client timeout of 60 s.
- Large screenshots: crop first, send only the selected region.
- Overlay left on screen: `Ctrl+Shift+X` always hides it.

---

## 13. Security and privacy

- The API key lives only on the user's machine. Upgrade to the Windows Credential Manager.
- The only data sent out is the cropped screenshot, the extracted question, and the retrieved RAG chunks, and only to the provider the user configured.
- No telemetry.
- The app never injects input into or controls other windows.

---

## 14. Build and run (Windows)

1. Install Rust (rustup) and the Microsoft C++ Build Tools. WebView2 ships with Windows 11.
2. `cargo install tauri-cli --version "^2"`
3. Generate icons: `cargo tauri icon path/to/logo.png` (creates `src-tauri/icons/`).
4. Dev run: `cd snipbots/src-tauri && cargo tauri dev`
5. Release build: `cargo tauri build`

---

## 15. Milestones (suggested order for the implementer)

1. **Skeleton**: Tauri v2 project, two windows, settings UI loads and saves `Config` to disk.
2. **Provider layer**: `chat()` for OpenAI-compatible endpoints with a text-only test button, then add image input, `list_models`, then the Anthropic path.
3. **Capture and snip**: hotkey, full-screen capture before showing overlay, drag selection, crop, PNG, vision call, print the extracted text in a bubble (no animations yet).
4. **Answer bot without RAG**: second call to answer the extracted text, show it in the bubble, set click-through.
5. **RAG**: chunker, folder ingest, BM25 retrieval, prompt assembly, chunk count in settings.
6. **Embeddings**: optional vector mode with cosine similarity.
7. **Bot animations**: SVG robots, fly-in and fly-out transitions per stage, error styling, bubble edge handling.
8. **Polish**: error messages, clear index, hotkey conflicts, icons, installer.

## 16. Acceptance tests

1. Open a page containing "What is the green planet?", press `Ctrl+Shift+S`, drag around the text. Snip Bot flies in, then Answer Bot, and the bubble shows "Earth". The user can then type in a text field underneath the overlay without it blocking input.
2. Put a note file with a made-up fact ("The class mascot is a purple otter") in the knowledge folder, build the index, and snip the question "What is the class mascot?". The answer is "A purple otter" (proves RAG is being used).
3. Remove the API key and snip: a readable error bubble appears and the app does not crash.
4. On a 150% DPI display, the cropped region matches what the user selected.
5. `Ctrl+Shift+X` always clears the overlay.
