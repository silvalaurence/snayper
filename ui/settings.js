const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);
const els = {
  led: $("led"),
  subtitle: $("subtitle"),
  status: $("status"),
  key: $("api_key"),
  folder: $("folder"),
  visionRow: $("vision_row"),
  answerRow: $("answer_row"),
  visionName: $("vision_name"),
  answerName: $("answer_name"),
  search: $("model_search"),
  list: $("model_list"),
  providers: $("providers"),
  providerRow: $("provider_row"),
  providerName: $("provider_name"),
  build: $("build"),
  watch: $("watch"),
  stopwatch: $("stopwatch"),
  snip: $("snip"),
  quit: $("quit"),
  toggle: $("toggle"),
};

let config = {};
let models = [];
let provider = "opencode";
let expanded = false;
let busy = false;
let watching = false;
let collapseTimer = 0;
let picked = { vision: false, answer: false };
let chosen = { vision: "", answer: "" };
let picking = null;

/* ---------- chrome ---------- */

function setStatus(text, kind = "") {
  els.status.textContent = text;
  els.status.classList.toggle("err", kind === "err");
  const watchLed = watching && kind !== "busy" && kind !== "err" ? " watch" : "";
  els.led.className = (kind ? `led ${kind}` : "led") + watchLed;
  if (els.subtitle.textContent !== text) {
    els.subtitle.textContent = text;
    els.subtitle.animate(
      [
        { opacity: 0, transform: "translateY(3px)", filter: "blur(3px)" },
        { opacity: 1, transform: "translateY(0)", filter: "blur(0)" },
      ],
      { duration: 220, easing: "ease-out" }
    );
  }
}

function setWatching(active) {
  watching = active;
  document.body.classList.toggle("watching", active);
  els.watch.classList.toggle("on", active);
  els.watch.title = active
    ? "Stop watching this box"
    : "Watch this snip box and answer new text automatically";
}

function setBusy(value) {
  busy = value;
  els.build.disabled = value;
  els.snip.disabled = value;
}

async function setExpanded(next) {
  expanded = next;
  clearTimeout(collapseTimer);
  document.body.classList.toggle("expanded", next);

  // The shape springs between explicit pixel heights; the window follows.
  // Growing: the window opens first, the island springs inside it.
  // Shrinking: the window waits until the content has cleared and the
  // shape has settled, so nothing is ever clipped mid-animation.
  const island = document.getElementById("island");
  if (next) {
    island.style.height = "auto";
    document.body.classList.add("measuring");
    const height = Math.ceil(island.getBoundingClientRect().height);
    document.body.classList.remove("measuring");
    island.style.height = `${height}px`;
    try {
      await invoke("resize_island", { height: height + 20 });
    } catch {}
  } else {
    island.style.height = "56px";
    collapseTimer = setTimeout(() => {
      if (!expanded) invoke("resize_island", { height: 76 }).catch(() => {});
    }, 520);
  }
}

const PROVIDER_NAMES = {
  opencode: "Zen",
  openrouter: "OpenRouter",
  openai: "OpenAI",
  anthropic: "Claude",
  gemini: "Gemini",
};

function setProvider(name) {
  provider = name;
  els.providerName.textContent = PROVIDER_NAMES[name] || name;
  for (const button of els.providers.querySelectorAll("button")) {
    button.classList.toggle("on", button.dataset.provider === name);
  }
}

/* ---------- model picking ---------- */

function guessProvider(key) {
  if (/^sk-ant-/.test(key)) return "anthropic";
  if (/^sk-or-v1-/.test(key)) return "openrouter";
  if (/^sk-(proj|svcacct|admin)-/.test(key)) return "openai";
  if (/^oc_/.test(key)) return "opencode";
  if (/^AIza|^AQ\./.test(key)) return "gemini";
  return null;
}

function score(id, kind) {
  const name = id.toLowerCase();
  let value = 0;
  const add = (pattern, points) => {
    if (pattern.test(name)) value += points;
  };

  if (kind === "vision") {
    // Models whose name advertises image input win over everything else.
    add(/-vl\b|super-vl|vision|omni|multimodal/, 60);
    add(/claude/, 40);
    add(/gpt-5/, 38);
    add(/gpt-4\.1/, 32);
    add(/gpt-4o/, 30);
    add(/gemini/, 34);
    add(/flash/, 4);
    add(/o4/, 22);
    add(/opus/, 6);
    add(/sonnet/, 4);
    if (/haiku|mini|nano|lite|small/.test(name)) value -= 6;
  } else {
    // Answers want speed and price over raw size.
    add(/lite|nano/, 30);
    add(/flash|mini|haiku/, 26);
    add(/claude|gpt-5|gpt-4\.1|gemini/, 12);
    if (/opus|pro|max|carbon|argon/.test(name)) value -= 6;
  }

  if (/embed|whisper|tts|dall|sora|moderation|realtime|audio|transcribe|rerank|image|computer-use|live|veo|imagen|aqa|gemma|learnlm|robotics|guard|deep-research|omni/.test(name)) {
    value -= 100;
  }
  // Newer generations are usually better; gemini-3.8 beats gemini-2.5.
  const version = name.match(/gemini-(\d+(?:\.\d+)?)/);
  if (version) value += (Number(version[1]) - 3) * 4;
  if (/preview|exp\b|experimental/.test(name)) value -= 4;
  return value;
}

function pick(list, kind) {
  // When a provider offers free models, prefer them; a zero-cost answer beats a nicer name.
  const isFree = (id) => /-free$|:free$/.test(id);
  const hasFree = list.some(isFree);
  const value = (id) => score(id, kind) + (hasFree && isFree(id) ? 30 : 0);

  const usable = list.filter((id) => score(id, kind) > -50);
  if (!usable.length) return list[0] ?? "";
  return usable.reduce((best, id) => (value(id) > value(best) ? id : best));
}

function renderModelRows() {
  els.visionName.textContent = chosen.vision || "auto";
  els.answerName.textContent = chosen.answer || "auto";
}

function clearModels() {
  models = [];
  chosen = { vision: "", answer: "" };
  picked = { vision: false, answer: false };
  renderModelRows();
  if (picking) renderList();
}

/* ---------- searchable model picker ---------- */

const TICK =
  '<svg class="tick" viewBox="0 0 12 12" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"><path d="M2.5 6.5 L5 9 L9.5 3"/></svg>';

function renderList() {
  const query = els.search.value.trim().toLowerCase();
  els.list.innerHTML = "";
  if (!models.length) {
    const empty = document.createElement("div");
    empty.className = "pempty";
    empty.textContent = "Connect a key to list models";
    els.list.append(empty);
    return;
  }
  const hits = query ? models.filter((id) => id.toLowerCase().includes(query)) : models;
  if (!hits.length) {
    const empty = document.createElement("div");
    empty.className = "pempty";
    empty.textContent = "No match";
    els.list.append(empty);
    return;
  }
  for (const id of hits) {
    const button = document.createElement("button");
    button.dataset.id = id;
    button.classList.toggle("on", id === chosen[picking]);
    const name = document.createElement("span");
    name.textContent = id;
    button.append(name);
    button.insertAdjacentHTML("beforeend", TICK);
    els.list.append(button);
  }
}

async function openPicker(field) {
  if (picking === field) {
    closePicker();
    return;
  }
  picking = field;
  document.body.classList.add("picker-open");
  els.visionRow.classList.toggle("open", field === "vision");
  els.answerRow.classList.toggle("open", field === "answer");
  els.search.value = "";
  renderList();
  if (expanded) setExpanded(true);
  els.search.focus();

  if (!models.length && els.key.value.trim()) {
    els.list.innerHTML = '<div class="pempty">Finding models…</div>';
    try {
      const list = await invoke("list_models");
      models = [...new Set(list)].sort();
    } catch {}
    if (picking === field) {
      renderList();
      if (expanded) setExpanded(true);
    }
  }
}

function closePicker() {
  picking = null;
  document.body.classList.remove("picker-open");
  els.visionRow.classList.remove("open");
  els.answerRow.classList.remove("open");
  if (expanded) setExpanded(true);
}

/* ---------- config ---------- */

async function saveConfig() {
  config.provider = provider;
  config.api_key = els.key.value.trim();
  config.vision_model = chosen.vision;
  config.answer_model = chosen.answer;
  await invoke("save_config", { config });
}

function isReady() {
  return Boolean(config.api_key && config.vision_model && config.answer_model);
}

/* ---------- actions ---------- */

async function connect({ detect = true } = {}) {
  if (busy) return;
  const key = els.key.value.trim();
  if (!key) {
    setStatus("Paste an API key first", "err");
    return;
  }

  if (detect) {
    const guess = guessProvider(key);
    if (guess && guess !== provider) {
      setProvider(guess);
      clearModels();
    }
  }

  setBusy(true);
  setStatus("Finding models…", "busy");

  try {
    await saveConfig();
    const list = await invoke("list_models");
    models = [...new Set(list)].sort();

    const vision =
      picked.vision && models.includes(chosen.vision) ? chosen.vision : pick(models, "vision");
    const answer =
      picked.answer && models.includes(chosen.answer) ? chosen.answer : pick(models, "answer");

    chosen = { vision, answer };
    picked = { vision: false, answer: false };
    renderModelRows();
    if (picking) renderList();

    await saveConfig();
    setStatus(`Ready · ${models.length} models`, "ok");
  } catch (error) {
    setStatus(String(error), "err");
    await setExpanded(true);
  } finally {
    setBusy(false);
  }
}

async function buildKnowledge() {
  if (busy) return;
  const folder = els.folder.value.trim();
  if (!folder) {
    setStatus("Choose a notes folder first", "err");
    return;
  }

  localStorage.setItem("snipbots.folder", folder);
  setBusy(true);
  setStatus("Building knowledge base…", "busy");

  try {
    await saveConfig();
    const count = await invoke("ingest_folder", { path: folder });
    setStatus(`Knowledge base · ${count} chunks`, "ok");
  } catch (error) {
    setStatus(String(error), "err");
  } finally {
    setBusy(false);
  }
}

/* ---------- wiring ---------- */

els.toggle.addEventListener("click", () => setExpanded(!expanded));

els.providerRow.addEventListener("click", () => {
  document.body.classList.toggle("providers-open");
  if (expanded) setExpanded(true);
});

els.providers.addEventListener("click", async (event) => {
  const button = event.target.closest("button[data-provider]");
  if (!button || button.dataset.provider === provider) return;

  setProvider(button.dataset.provider);
  document.body.classList.remove("providers-open");
  if (expanded) setExpanded(true);
  clearModels();

  try {
    await saveConfig();
    if (els.key.value.trim()) await connect({ detect: false });
    else setStatus("Paste a key to find your models");
  } catch (error) {
    setStatus(String(error), "err");
  }
});

els.key.addEventListener("keydown", (event) => {
  if (event.key === "Enter") connect();
});

els.key.addEventListener("paste", () => setTimeout(() => connect(), 60));

els.visionRow.addEventListener("click", () => openPicker("vision"));
els.answerRow.addEventListener("click", () => openPicker("answer"));

els.search.addEventListener("input", renderList);
els.search.addEventListener("keydown", (event) => {
  if (event.key === "Enter") els.list.querySelector("button")?.click();
  if (event.key === "Escape") closePicker();
});

els.list.addEventListener("click", async (event) => {
  const button = event.target.closest("button[data-id]");
  if (!button || !picking) return;
  chosen[picking] = button.dataset.id;
  picked[picking] = true;
  renderModelRows();
  closePicker();
  await saveConfig();
  setStatus("Saved", "ok");
});

els.build.addEventListener("click", buildKnowledge);

els.watch.addEventListener("click", async () => {
  try {
    await invoke("toggle_watch");
  } catch (error) {
    setStatus(String(error), "err");
  }
});

els.stopwatch.addEventListener("click", async () => {
  try {
    await invoke("toggle_watch");
  } catch (error) {
    setStatus(String(error), "err");
  }
});

listen("watch-state", (event) => {
  const { active, text } = event.payload ?? {};
  setWatching(Boolean(active));
  setStatus(text || (active ? "Watching" : "Ready"), active ? "ok" : "");
});

els.snip.addEventListener("click", async () => {
  setStatus("Snip started · drag on screen", "busy");
  try {
    await invoke("start_snip");
  } catch (error) {
    setStatus(String(error), "err");
  }
  setTimeout(() => {
    if (watching) setStatus("Watching · Ctrl Shift A to stop", "ok");
    else if (isReady()) setStatus("Ready", "ok");
    else setStatus("Add your API key");
  }, 800);
});

els.quit.addEventListener("click", () => invoke("quit_app"));

/* ---------- boot ---------- */

(async function boot() {
  setExpanded(false);
  try {
    config = await invoke("get_config");
    setProvider(config.provider || "opencode");
    els.key.value = config.api_key || "";
    els.folder.value = localStorage.getItem("snipbots.folder") || "";

    if (config.vision_model) chosen.vision = config.vision_model;
    if (config.answer_model) chosen.answer = config.answer_model;
    renderModelRows();

    const chunks = await invoke("rag_stats");
    if (chunks > 0) setStatus(`Knowledge base · ${chunks} chunks`, "ok");
    else if (isReady()) setStatus("Ready", "ok");
    else setStatus("Paste a key to find your models");

    // The key itself tells us the provider: a saved key always wins over a stale choice.
    const guess = guessProvider(config.api_key || "");
    if (guess && guess !== provider) {
      setProvider(guess);
      clearModels();
      setStatus("Finding models…", "busy");
      setTimeout(connect, 200);
    } else if (config.api_key && !config.vision_model) {
      setTimeout(connect, 400);
    }
  } catch (error) {
    setStatus(String(error), "err");
  }
})();
