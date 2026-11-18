# Snayper

*Press a hotkey. Drag a box. Two robots handle the rest — you just have to type the answer yourself.*

Snayper is a tiny always-on-top island for Windows that reads whatever text you frame on screen and answers it. It never clicks, never types into other apps, never quietly "solves" anything on your behalf. It just reads, thinks, and hands you the answer in a bubble. A very nosy study buddy that respects your keyboard.

<p align="center">
  <img src="src-tauri/icons/128x128@2x.png" width="128" alt="Snayper island icon">
</p>

## Meet the crew

| Robot | Colour | Job |
|---|---|---|
| **Read Bot** | blue, fast, slightly nosy | snips the box, extracts the text with a vision model |
| **Answer Bot** | orange, overthinks everything | answers using your own key and (optionally) your own notes |

They live in `ui/assets/` as Lottie animations and float politely outside the watched area, because a robot sitting *inside* the watched box would make Snayper answer its own shadow. (It tried. It was embarrassing.)

## The party trick

Snip once.

Snayper keeps watching that exact box. When the question inside it changes, Read Bot wakes up and Answer Bot answers — no second snip, no five-second apology tour, no "did you mean to re-read that?" Snip a different box and the watch follows you there.

Answers stream into the bubble as they're written, so you can start reading before Answer Bot has finished being dramatic.

## Hotkeys

| Keys | What happens |
|---|---|
| `Ctrl+Shift+S` | snip a box (and make it the watched box) |
| `Ctrl+Shift+A` | start / stop watching |
| `Ctrl+Shift+X` | clear the answer bubble |
| `Ctrl+Shift+H` | hide / show the island |
| `Esc` | cancel a snip in progress |

## Stealth mode

Snayper calls `SetWindowDisplayAffinity` with `WDA_EXCLUDEFROMCAPTURE`, which is the OS politely agreeing to pretend the app doesn't exist. Screen shares, recordings, Teams/Meet/Discord calls and screenshot tools never see the island, the outline, the robots, or the bubble.

A phone camera pointed at your screen still sees everything. This blocks software capture, not optics. Physics remains undefeated.

## Getting it running

You'll need:

- Windows 10/11 (capture exclusion needs 10 2004+)
- Rust via [rustup](https://rustup.rs)
- A C++ linker: MSVC Build Tools, or the `x86_64-pc-windows-gnu` target
- WebView2 (built into Windows 11; install the Evergreen runtime on Windows 10)

Development build:

```powershell
cd src-tauri
cargo run
```

A real, shippable exe:

```powershell
cargo install tauri-cli --version "^2"
cd src-tauri
cargo tauri build
```

Then:

1. Click the chevron to open the island.
2. Paste an API key. Snayper sniffs out the provider from the key and fetches the model list.
3. It picks a vision model and an answer model for you. Tap either row if you'd rather choose — there's search, because some providers have 284 models and all of them are named `vendor/model-7`.
4. Optional: point **Notes** at a folder of `.txt` / `.md` / `.csv` / `.json` files and hit **Build**. Answers get smarter; the robots get smug.
5. `Ctrl+Shift+S`, drag over a question, read the bubble, type the answer like a normal human.

## Providers and free stuff

| Provider | Talks | Watch out |
|---|---|---|
| OpenCode Zen | OpenAI-compatible | free models rotate; check what's live |
| OpenRouter | OpenAI-compatible | `:free` models are the budget hero: 20 req/min, ~50/day, reset at 00:00 UTC. One-time $10 tops you up to 1,000/day |
| OpenAI | OpenAI-compatible | your wallet, your business |
| Claude | native messages API | no embeddings endpoint, so notes fall back to keyword search |
| Gemini | OpenAI-compatible endpoint | free Flash is *very* free and very capped (around 20/day) |

Hit a cap and the island will say so, usually while sounding like a 429. Wait for the reset or pick a cheaper model.

## Notes (the fine print, but friendly)

- Only the primary monitor is captured in this version.
- The API key is stored in plain text in `config.json` inside the Tauri app config folder (`%APPDATA%\com.snipbots.app\`). It never leaves your machine except to your chosen provider.
- Your notes index lives in the same folder as `rag_index.json`. Delete it and rebuild whenever.
- No accounts, no telemetry, no servers of ours in the middle.
- Drag the island by its top bar if it's in your way. Or hit `Ctrl+Shift+H` and let it nap.

## How it works, roughly

```
hotkey → hide overlay → capture primary monitor → show overlay
       → drag a rectangle → crop
       → vision model reads the text
       → keyword (BM25) or embedding search over your notes
       → answer model replies → click-through bubble
```

`ARCHITECTURE.md` has the full tour if you like diagrams with your coffee.

---

There's no license file here — it's a personal project, so if you want to use it for something, just ask. The robots are friendlier than the legal department.
