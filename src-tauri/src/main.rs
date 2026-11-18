#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod capture;
mod config;
mod providers;
mod rag;

use std::sync::Mutex;
use std::time::Duration;

use anyhow::{anyhow, Result};
use serde::Deserialize;
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, PhysicalSize, WebviewWindow};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, ShortcutState};

use config::Config;

const SNIP_PROMPT: &str = "You are an OCR assistant. Transcribe all text in the image exactly, including question numbers and answer options. Output only the text, no commentary.";

struct AppState {
    cfg: Mutex<Config>,
    shot: Mutex<Option<capture::Shot>>,
    index: Mutex<rag::Index>,
    watch: Mutex<Option<Watch>>,
    last_rect: Mutex<Option<Rect>>,
}

/// Live "answer new text automatically" mode: re-checks one screen box and
/// only calls the model when its pixels actually change.
struct Watch {
    rect: Rect,
    epoch: u64,
    last_hash: u64,
    last_question: String,
    job_id: u64,
    job: Option<tauri::async_runtime::JoinHandle<()>>,
}

impl Watch {
    fn abort_job(&mut self) {
        if let Some(job) = self.job.take() {
            job.abort();
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, serde::Serialize)]
struct Rect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    dpr: f64,
}

impl Rect {
    fn px(&self) -> (i64, i64, u32, u32) {
        let dpr = if self.dpr > 0.0 { self.dpr } else { 1.0 };
        (
            (self.x * dpr).round() as i64,
            (self.y * dpr).round() as i64,
            (self.w * dpr).round().max(1.0) as u32,
            (self.h * dpr).round().max(1.0) as u32,
        )
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn overlay(app: &AppHandle) -> Option<WebviewWindow> {
    app.get_webview_window("overlay")
}

fn emit_stage(app: &AppHandle, stage: &str, text: &str) {
    let _ = app.emit_to(
        "overlay",
        "stage",
        serde_json::json!({ "stage": stage, "text": text }),
    );
}

/// Append a line to `app.log` in the app config folder. This is the only
/// durable trace of what the pipeline did, since there is no console.
fn log(app: &AppHandle, message: impl AsRef<str>) {
    use std::io::Write;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    if let Ok(dir) = app.path().app_config_dir() {
        let _ = std::fs::create_dir_all(&dir);
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("app.log"))
        {
            let _ = writeln!(file, "[{stamp}] {}", message.as_ref());
        }
    }
}

fn config_dir(app: &AppHandle) -> Option<std::path::PathBuf> {
    app.path().app_config_dir().ok()
}

/// Keep Snayper's own windows out of screen captures: the watch hashes the
/// watched box's pixels, and any overlay tint, bot or shadow inside it would
/// look like "new text" forever. WDA_EXCLUDEFROMCAPTURE (Win10 2004+).
#[cfg(windows)]
fn exclude_from_capture(win: &WebviewWindow) -> bool {
    #[link(name = "user32")]
    extern "system" {
        fn SetWindowDisplayAffinity(hwnd: isize, affinity: u32) -> i32;
    }
    match win.hwnd() {
        Ok(hwnd) => unsafe { SetWindowDisplayAffinity(hwnd.0 as isize, 0x11) != 0 },
        Err(_) => false,
    }
}

#[cfg(not(windows))]
fn exclude_from_capture(_win: &WebviewWindow) -> bool {
    false
}

/// Size and position the overlay over the primary monitor.
fn cover_primary_monitor(app: &AppHandle, win: &WebviewWindow) -> Result<()> {
    let monitor = app
        .primary_monitor()?
        .ok_or_else(|| anyhow!("no primary monitor found"))?;
    let position = monitor.position();
    let size = monitor.size();
    win.set_position(PhysicalPosition::new(position.x, position.y))?;
    win.set_size(PhysicalSize::new(size.width, size.height))?;
    Ok(())
}

async fn begin_snip(app: &AppHandle) -> Result<()> {
    let win = overlay(app).ok_or_else(|| anyhow!("overlay window is missing"))?;

    // Capture before the overlay shows so the dim layer is never in the shot.
    let _ = win.hide();
    tokio::time::sleep(Duration::from_millis(150)).await;

    let shot = capture::capture_primary()?;
    log(
        app,
        format!("capture: {}x{}", shot.width, shot.height),
    );
    *lock(&app.state::<AppState>().shot) = Some(shot);

    cover_primary_monitor(app, &win)?;
    win.set_ignore_cursor_events(false)?;
    win.show()?;
    win.set_focus()?;
    let _ = app.emit_to("overlay", "snip-start", ());
    Ok(())
}

async fn run_start_snip(app: AppHandle) {
    if let Err(err) = begin_snip(&app).await {
        log(&app, format!("capture failed: {err:#}"));
        if let Some(win) = overlay(&app) {
            let _ = cover_primary_monitor(&app, &win);
            let _ = win.set_ignore_cursor_events(true);
            let _ = win.show();
        }
        emit_stage(&app, "error", &format!("{err:#}"));
    }
}

fn hide(app: &AppHandle) {
    if let Some(win) = overlay(app) {
        let _ = win.set_ignore_cursor_events(false);
        let _ = win.hide();
    }
    let _ = app.emit_to("overlay", "reset", ());
}

/* ---------- watch mode: answer new text automatically ---------- */

static WATCH_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn watch_status(app: &AppHandle, active: bool, text: &str) {
    let _ = app.emit_to(
        "main",
        "watch-state",
        serde_json::json!({ "active": active, "text": text }),
    );
}

#[tauri::command]
async fn toggle_watch(app: AppHandle) -> Result<bool, String> {
    let state = app.state::<AppState>();

    let was_active = {
        let mut watch = lock(&state.watch);
        match watch.take() {
            Some(mut w) => {
                w.abort_job();
                true
            }
            None => false,
        }
    };

    if was_active {
        log(&app, "watch: stopped");
        let _ = app.emit_to("overlay", "reset", ());
        if let Some(win) = overlay(&app) {
            let _ = win.hide();
        }
        watch_status(&app, false, "Watch off");
        return Ok(false);
    }

    let rect = *lock(&state.last_rect);
    let Some(rect) = rect else {
        watch_status(&app, false, "Snip a box once, then Ctrl Shift A");
        return Err("snip a question once first, then press Ctrl+Shift+A".into());
    };

    start_watch(&app, rect, 0);
    Ok(true)
}

/// Begin watching `rect`. `seed_hash` skips the frame the user just snipped,
/// so only genuinely new pixels trigger a model call.
fn start_watch(app: &AppHandle, rect: Rect, seed_hash: u64) {
    let epoch = WATCH_EPOCH.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
    let previous = lock(&app.state::<AppState>().watch).replace(Watch {
        rect,
        epoch,
        last_hash: seed_hash,
        last_question: String::new(),
        job_id: 0,
        job: None,
    });
    if let Some(mut old) = previous {
        old.abort_job();
    }

    log(app, "watch: started");
    watch_status(app, true, "Watching · Ctrl Shift A to stop");
    let _ = app.emit_to("overlay", "watch", serde_json::json!({ "rect": rect }));

    let handle = app.clone();
    tauri::async_runtime::spawn(async move { watch_loop(handle, epoch).await });
}

async fn watch_loop(app: AppHandle, epoch: u64) {
    if let Some(win) = overlay(&app) {
        let _ = cover_primary_monitor(&app, &win);
        let _ = win.set_ignore_cursor_events(true);
        let _ = win.show();
    }

    loop {
        tokio::time::sleep(Duration::from_millis(250)).await;

        let current = {
            let state = app.state::<AppState>();
            let watch = lock(&state.watch);
            match watch.as_ref() {
                Some(w) if w.epoch == epoch => Some((w.rect, w.last_hash)),
                _ => None,
            }
        };
        let Some((rect, last_hash)) = current else {
            break;
        };

        // Only the watched box is grabbed, straight off the screen; no
        // full-monitor capture and no capture session setup per poll.
        let (x, y, w, h) = rect.px();
        let shot = match capture::capture_region(x, y, w, h) {
            Ok(shot) => shot,
            Err(err) => {
                log(&app, format!("watch: capture failed: {err:#}"));
                continue;
            }
        };

        let hash = capture::crop_hash(&shot, 0, 0, shot.width, shot.height);
        if hash == last_hash {
            continue;
        }

        // Remember the frame before the model call so a slow answer cannot
        // trigger the same frame twice.
        {
            let state = app.state::<AppState>();
            let mut watch = lock(&state.watch);
            match watch.as_mut() {
                Some(w) if w.epoch == epoch => w.last_hash = hash,
                _ => break,
            }
        }

        let cfg = lock(&app.state::<AppState>().cfg).clone();
        if cfg.api_key.trim().is_empty() || cfg.vision_model.trim().is_empty() {
            {
                let state = app.state::<AppState>();
                let mut watch = lock(&state.watch);
                if let Some(mut w) = watch.take() {
                    w.abort_job();
                }
            }
            watch_status(&app, false, "Set your API key and models first");
            let _ = app.emit_to("overlay", "reset", ());
            break;
        }

        let bytes = match capture::crop_png(&shot, 0, 0, shot.width, shot.height) {
            Ok(bytes) => bytes,
            Err(_) => continue,
        };
        let image = capture::to_b64(&bytes);

        // The loop never waits on the model: a new change aborts the stale
        // job and starts reading the newest frame right away.
        let state = app.state::<AppState>();
        let mut watch = lock(&state.watch);
        match watch.as_mut() {
            Some(w) if w.epoch == epoch => {
                w.abort_job();
                let job_id = WATCH_JOB.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                w.job_id = job_id;
                log(&app, format!("watch: pixels changed, job {job_id}"));
                let handle = app.clone();
                w.job = Some(tauri::async_runtime::spawn(async move {
                    watch_job(handle, epoch, job_id, rect, cfg, image).await;
                }));
            }
            _ => break,
        }
    }
}

static WATCH_JOB: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn job_current(app: &AppHandle, epoch: u64, job_id: u64) -> bool {
    let state = app.state::<AppState>();
    let watch = lock(&state.watch);
    matches!(watch.as_ref(), Some(w) if w.epoch == epoch && w.job_id == job_id)
}

async fn watch_job(
    app: AppHandle,
    epoch: u64,
    job_id: u64,
    rect: Rect,
    cfg: Config,
    image: String,
) {
    let question = match providers::chat(
        &cfg,
        SNIP_PROMPT,
        "Extract the text.",
        Some(&image),
        &cfg.vision_model,
    )
    .await
    {
        Ok(text) => text.trim().to_string(),
        Err(err) => {
            log(&app, format!("watch: vision failed: {err:#}"));
            return;
        }
    };

    if question.is_empty() || !job_current(&app, epoch, job_id) {
        return;
    }

    let already_seen = {
        let state = app.state::<AppState>();
        let mut watch = lock(&state.watch);
        match watch.as_mut() {
            Some(w) if w.epoch == epoch && w.job_id == job_id => {
                let seen = w.last_question == question;
                w.last_question = question.clone();
                seen
            }
            _ => return,
        }
    };
    if already_seen {
        return;
    }

    log(
        &app,
        format!("watch: new text ({} chars)", question.chars().count()),
    );

    if let Some(win) = overlay(&app) {
        let _ = cover_primary_monitor(&app, &win);
        let _ = win.set_ignore_cursor_events(true);
        let _ = win.show();
    }
    let _ = app.emit_to("overlay", "watch", serde_json::json!({ "rect": rect }));
    emit_stage(&app, "read", &question);

    let result = reply(&app, &cfg, &question).await;
    if !job_current(&app, epoch, job_id) {
        return;
    }
    match result {
        Ok(answer) => emit_stage(&app, "answer", &answer),
        Err(err) => emit_stage(&app, "error", &format!("{err:#}")),
    }
}

async fn pipeline(app: &AppHandle, rect: &Rect) -> Result<()> {
    if rect.w < 5.0 || rect.h < 5.0 {
        return Err(anyhow!("the selection is too small. Drag over the question."));
    }

    if let Some(win) = overlay(app) {
        let _ = win.set_ignore_cursor_events(true);
    }

    let cfg = lock(&app.state::<AppState>().cfg).clone();
    if cfg.api_key.trim().is_empty() {
        return Err(anyhow!("set your API key in Snayper settings."));
    }
    if cfg.vision_model.trim().is_empty() {
        return Err(anyhow!("set a vision model in Snayper settings."));
    }

    let png = {
        let state = app.state::<AppState>();
        let shot = lock(&state.shot);
        let shot = shot
            .as_ref()
            .ok_or_else(|| anyhow!("no screenshot yet. Press Ctrl+Shift+S to start a snip."))?;

        let (x, y, w, h) = rect.px();

        log(
            app,
            format!(
                "snip: screen {}x{}, sel {:.0},{:.0} {:.0}x{:.0} dpr {} -> crop {x},{y} {w}x{h}",
                shot.width, shot.height, rect.x, rect.y, rect.w, rect.h, rect.dpr
            ),
        );

        let bytes = capture::crop_png(shot, x, y, w, h)?;
        if let Some(dir) = config_dir(app) {
            let _ = std::fs::write(dir.join("last_snip.png"), &bytes);
        }
        log(app, format!("snip: png {} bytes", bytes.len()));
        capture::to_b64(&bytes)
    };

    emit_stage(app, "snip", "");

    let started = std::time::Instant::now();
    let question = providers::chat(
        &cfg,
        SNIP_PROMPT,
        "Extract the text.",
        Some(&png),
        &cfg.vision_model,
    )
    .await
    .map_err(|err| {
        log(
            app,
            format!(
                "vision {} failed after {:?}: {err:#}",
                cfg.vision_model,
                started.elapsed()
            ),
        );
        err
    })?;
    log(
        app,
        format!(
            "vision {} ok in {:?}",
            cfg.vision_model,
            started.elapsed()
        ),
    );
    let question = question.trim().to_string();
    if question.is_empty() {
        return Err(anyhow!("no text was found in the selection."));
    }
    {
        // The live watch must not answer the question this snip is answering.
        let state = app.state::<AppState>();
        let mut watch = lock(&state.watch);
        if let Some(w) = watch.as_mut() {
            w.last_question = question.clone();
        }
    }
    emit_stage(app, "read", &question);

    let answer = reply(app, &cfg, &question).await?;
    emit_stage(app, "answer", &answer);
    Ok(())
}

/// Notes retrieval + the answer call. Shared by the manual snip and watch mode.
async fn reply(app: &AppHandle, cfg: &Config, question: &str) -> Result<String> {
    if cfg.answer_model.trim().is_empty() {
        return Err(anyhow!("set an answer model in Snayper settings."));
    }

    // Optional vector search. Any failure falls back to keyword search.
    let mut query_embedding = None;
    if cfg.embeddings_enabled() {
        if let Ok(mut vectors) = providers::embed(cfg, std::slice::from_ref(&question.to_string())).await {
            if let Some(vector) = vectors.pop() {
                query_embedding = Some(vector);
            }
        }
    }

    let hits = {
        let state = app.state::<AppState>();
        let index = lock(&state.index);
        rag::retrieve(&index, question, cfg.top_k, query_embedding.as_deref())
    };

    let context = hits
        .iter()
        .enumerate()
        .map(|(i, chunk)| format!("[{}] ({}) {}", i + 1, chunk.source, chunk.text))
        .collect::<Vec<_>>()
        .join("\n\n");

    let user = if context.is_empty() {
        format!("Question:\n{question}")
    } else {
        format!(
            "Context (from the user's notes, may be irrelevant):\n{context}\n\nQuestion:\n{question}"
        )
    };

    let started = std::time::Instant::now();
    let answer = providers::chat(
        cfg,
        &cfg.system_prompt,
        &user,
        None,
        &cfg.answer_model,
    )
    .await
    .map_err(|err| {
        log(
            app,
            format!(
                "answer {} failed after {:?}: {err:#}",
                cfg.answer_model,
                started.elapsed()
            ),
        );
        err
    })?;

    log(
        app,
        format!(
            "answer {} ok in {:?} ({} chars)",
            cfg.answer_model,
            started.elapsed(),
            answer.trim().chars().count()
        ),
    );
    Ok(answer.trim().to_string())
}

#[tauri::command]
fn get_config(app: AppHandle) -> Config {
    // Read from disk so the first paint never races app startup.
    Config::load(&app)
}

#[tauri::command]
fn save_config(app: AppHandle, config: Config) -> Result<(), String> {
    config.save(&app).map_err(|err| format!("{err:#}"))?;
    if let Some(state) = app.try_state::<AppState>() {
        *lock(&state.cfg) = config;
    }
    Ok(())
}

#[tauri::command]
async fn list_models(app: AppHandle) -> Result<Vec<String>, String> {
    let cfg = Config::load(&app);
    providers::list_models(&cfg)
        .await
        .map_err(|err| format!("{err:#}"))
}

#[tauri::command]
async fn ingest_folder(app: AppHandle, path: String) -> Result<usize, String> {
    let cfg = Config::load(&app);
    let chunks = rag::read_folder(&path).map_err(|err| format!("{err:#}"))?;
    if chunks.is_empty() {
        return Err("no readable text files were found in that folder".into());
    }

    let mut index = rag::Index { chunks };

    if cfg.embeddings_enabled() {
        let texts: Vec<String> = index.chunks.iter().map(|c| c.text.clone()).collect();
        for (batch_number, batch) in texts.chunks(32).enumerate() {
            let vectors = providers::embed(&cfg, batch)
                .await
                .map_err(|err| format!("embedding failed: {err:#}"))?;
            for (offset, vector) in vectors.into_iter().enumerate() {
                index.chunks[batch_number * 32 + offset].embedding = Some(vector);
            }
        }
    }

    let count = index.chunks.len();
    rag::save(&app, &index).map_err(|err| format!("{err:#}"))?;
    if let Some(state) = app.try_state::<AppState>() {
        *lock(&state.index) = index;
    }
    Ok(count)
}

#[tauri::command]
fn rag_stats(app: AppHandle) -> usize {
    rag::load(&app).chunks.len()
}

#[tauri::command]
fn clear_index(app: AppHandle) -> Result<(), String> {
    rag::clear(&app).map_err(|err| format!("{err:#}"))?;
    if let Some(state) = app.try_state::<AppState>() {
        *lock(&state.index) = rag::Index::default();
    }
    Ok(())
}

#[tauri::command]
async fn start_snip(app: AppHandle) {
    run_start_snip(app).await;
}

#[tauri::command]
async fn process_snip(app: AppHandle, rect: Rect) -> Result<(), String> {
    let seed_hash = {
        let state = app.state::<AppState>();
        *lock(&state.last_rect) = Some(rect);
        let (x, y, w, h) = rect.px();
        let shot = lock(&state.shot);
        shot
            .as_ref()
            .map(|shot| capture::crop_hash(shot, x, y, w, h))
            .unwrap_or(0)
    };

    let ready = {
        let state = app.state::<AppState>();
        let cfg = lock(&state.cfg);
        !cfg.api_key.trim().is_empty()
            && !cfg.vision_model.trim().is_empty()
            && !cfg.answer_model.trim().is_empty()
    };

    let watching = {
        let state = app.state::<AppState>();
        let mut watch = lock(&state.watch);
        match watch.as_mut() {
            Some(w) => {
                // Re-target the live watch at the freshly drawn box; the old
                // frame's answer must not land on the new one.
                w.abort_job();
                w.rect = rect;
                w.last_hash = seed_hash;
                true
            }
            None => false,
        }
    };
    if !watching && ready {
        start_watch(&app, rect, seed_hash);
    }

    tauri::async_runtime::spawn(async move {
        if let Err(err) = pipeline(&app, &rect).await {
            log(&app, format!("pipeline error: {err:#}"));
            emit_stage(&app, "error", &format!("{err:#}"));
        }
    });
    Ok(())
}

#[tauri::command]
fn hide_overlay(app: AppHandle) {
    hide(&app);
}

const ISLAND_WIDTH: f64 = 440.0;
const ISLAND_COLLAPSED: f64 = 76.0;

#[tauri::command]
fn resize_island(app: AppHandle, height: f64) -> Result<(), String> {
    let win = app
        .get_webview_window("main")
        .ok_or_else(|| "main window is missing".to_string())?;
    let height = height.clamp(ISLAND_COLLAPSED, 900.0);
    win.set_size(tauri::LogicalSize::new(ISLAND_WIDTH, height))
        .map_err(|err| err.to_string())
}

#[tauri::command]
fn quit_app(app: AppHandle) {
    app.exit(0);
}

fn main() {
    tauri::Builder::default()
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, shortcut, event| {
                    if event.state() != ShortcutState::Pressed {
                        return;
                    }
                    if !shortcut.mods.contains(Modifiers::CONTROL)
                        || !shortcut.mods.contains(Modifiers::SHIFT)
                    {
                        return;
                    }

                    let app = app.clone();
                    match shortcut.key {
                        Code::KeyS => {
                            tauri::async_runtime::spawn(run_start_snip(app));
                        }
                        Code::KeyA => {
                            tauri::async_runtime::spawn(async move {
                                let _ = toggle_watch(app).await;
                            });
                        }
                        Code::KeyX => hide(&app),
                        Code::KeyH => {
                            if let Some(win) = app.get_webview_window("main") {
                                if win.is_visible().unwrap_or(false) {
                                    let _ = win.hide();
                                    log(&app, "island hidden (Ctrl Shift H)");
                                } else {
                                    let _ = win.show();
                                    log(&app, "island shown (Ctrl Shift H)");
                                }
                            }
                        }
                        _ => {}
                    }
                })
                .build(),
        )
        .setup(|app| {
            let handle = app.handle();
            app.manage(AppState {
                cfg: Mutex::new(Config::load(handle)),
                shot: Mutex::new(None),
                index: Mutex::new(rag::load(handle)),
                watch: Mutex::new(None),
                last_rect: Mutex::new(None),
            });

            if let Some(win) = app.get_webview_window("overlay") {
                let _ = win.set_ignore_cursor_events(true);
                log(handle, format!("capture exclusion overlay: {}", exclude_from_capture(&win)));
            }

            // Park the island at the top center of the screen.
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.set_skip_taskbar(true);
                let _ = win.center();
                if let (Ok(position), Ok(scale)) = (win.outer_position(), win.scale_factor()) {
                    let y = (56.0 * scale).round() as i32;
                    let _ = win.set_position(PhysicalPosition::new(position.x, y));
                }
                let excluded = exclude_from_capture(&win);
                log(handle, format!("capture exclusion island: {excluded}"));
            }

            for shortcut in ["Ctrl+Shift+S", "Ctrl+Shift+X", "Ctrl+Shift+A", "Ctrl+Shift+H"] {
                if let Err(err) = app.global_shortcut().register(shortcut) {
                    eprintln!("could not register {shortcut}: {err}");
                }
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_config,
            save_config,
            list_models,
            ingest_folder,
            rag_stats,
            clear_index,
            start_snip,
            process_snip,
            hide_overlay,
            resize_island,
            toggle_watch,
            quit_app
        ])
        .run(tauri::generate_context!())
        .expect("error while running Snayper");
}
