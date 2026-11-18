const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const dim = document.getElementById("dim");
const selEl = document.getElementById("sel");
const hint = document.getElementById("hint");
const stage = document.getElementById("stage");
const bubble = document.getElementById("bubble");

let selecting = false;
let anchor = { x: 0, y: 0 };
let selection = null;
let snipBot = null;
let answerBot = null;
let streaming = null;

const clamp = (value, min, max) => Math.min(max, Math.max(min, value));

function clearBubble() {
  streaming = null;
  bubble.classList.add("hidden");
  bubble.classList.remove("show", "error");
  bubble.innerHTML = "";
  bubble.style.left = "";
  bubble.style.top = "";
}

function resetOverlay() {
  selecting = false;
  selection = null;
  dim.classList.add("hidden");
  hint.classList.add("hidden");
  selEl.classList.add("hidden");
  selEl.classList.remove("flash", "locked", "watching");
  destroyBot(snipBot);
  destroyBot(answerBot);
  stage.replaceChildren();
  snipBot = null;
  answerBot = null;
  document.body.style.cursor = "default";
  clearBubble();
}

/* ---------- selection ---------- */

function rectFrom(ax, ay, bx, by) {
  return {
    x: Math.min(ax, bx),
    y: Math.min(ay, by),
    w: Math.abs(bx - ax),
    h: Math.abs(by - ay),
  };
}

function paintSelection(rect) {
  selEl.style.left = `${rect.x}px`;
  selEl.style.top = `${rect.y}px`;
  selEl.style.width = `${rect.w}px`;
  selEl.style.height = `${rect.h}px`;
}

window.addEventListener("mousedown", (event) => {
  if (event.button !== 0 || dim.classList.contains("hidden")) return;
  selecting = true;
  anchor = { x: event.clientX, y: event.clientY };
  paintSelection(rectFrom(anchor.x, anchor.y, anchor.x, anchor.y));
  selEl.classList.remove("hidden");
});

window.addEventListener("mousemove", (event) => {
  if (!selecting) return;
  paintSelection(rectFrom(anchor.x, anchor.y, event.clientX, event.clientY));
});

window.addEventListener("mouseup", (event) => {
  if (!selecting) return;
  selecting = false;

  const rect = rectFrom(anchor.x, anchor.y, event.clientX, event.clientY);
  if (rect.w < 5 || rect.h < 5) {
    selEl.classList.add("hidden");
    return; // keep the dim layer up so the user can try again
  }

  selection = rect;
  dim.classList.add("hidden");
  hint.classList.add("hidden");
  selEl.classList.add("hidden");
  document.body.style.cursor = "default";

  invoke("process_snip", {
    rect: { ...rect, dpr: window.devicePixelRatio || 1 },
  }).catch((error) => showError(String(error)));
});

window.addEventListener("keydown", (event) => {
  if (event.key === "Escape") {
    invoke("hide_overlay").catch(() => {});
  }
});

/* ---------- bots ---------- */

const SNIP_ROBOT = "assets/pm-robot.json";
const ANSWER_ROBOT = "assets/test-robot.json";

function spawnBot(src, from) {
  const el = document.createElement("div");
  el.className = "bot";
  const animBox = document.createElement("div");
  animBox.className = "bot-anim";
  el.appendChild(animBox);
  el._anim = lottie.loadAnimation({
    container: animBox,
    renderer: "svg",
    loop: true,
    autoplay: true,
    path: src,
  });
  el.style.transition = "none";
  el.style.left = `${from.x}px`;
  el.style.top = `${from.y}px`;
  el.style.opacity = "0";
  stage.appendChild(el);
  void el.offsetWidth; // force layout so the first move animates
  el.style.transition = "opacity 200ms ease";
  el.style.opacity = "1";
  return el;
}

function destroyBot(el) {
  if (el && el._anim) el._anim.destroy();
}

function moveBot(el, x, y, duration = 900) {
  el.style.transition = `left ${duration}ms cubic-bezier(.22,1,.36,1), top ${duration}ms cubic-bezier(.22,1,.36,1), opacity 220ms ease`;
  el.style.left = `${clamp(x, -200, window.innerWidth + 200)}px`;
  el.style.top = `${clamp(y, -200, window.innerHeight + 200)}px`;
}

function sendAway(el) {
  if (!el) return;
  // Fade in place: flying across the watched box would change its pixels.
  el.style.transition = "opacity 400ms ease";
  el.style.opacity = "0";
  setTimeout(() => {
    destroyBot(el);
    el.remove();
  }, 450);
}

// Bots must sit fully outside the watched box — their looping animation
// would otherwise look like "new text" to the pixel watcher.
function snipTarget() {
  if (!selection) return { x: 40, y: 40 };
  const above = selection.y - 104;
  return {
    x: clamp(selection.x - 78, 6, window.innerWidth - 102),
    y: above >= 6 ? above : clamp(selection.y + selection.h + 8, 6, window.innerHeight - 102),
  };
}

function answerTarget() {
  if (!selection) return { x: 80, y: 80 };
  const above = selection.y - 102;
  return {
    x: clamp(selection.x + selection.w - 48, 6, window.innerWidth - 102),
    y: above >= 6 ? above : clamp(selection.y + selection.h + 6, 6, window.innerHeight - 102),
  };
}

/* ---------- bubble ---------- */

function placeBubble() {
  if (!selection) return;

  // Fade in only on first appearance; later calls just keep it anchored.
  const first = !bubble.classList.contains("show");
  if (first) bubble.classList.remove("show");
  const rect = bubble.getBoundingClientRect();
  const gap = 16;

  let left = selection.x + selection.w + gap;
  if (left + rect.width > window.innerWidth - 10) {
    left = selection.x - rect.width - gap;
  }
  left = clamp(left, 10, Math.max(10, window.innerWidth - rect.width - 10));

  let top = selection.y + selection.h / 2 - rect.height / 2;
  top = clamp(top, 10, Math.max(10, window.innerHeight - rect.height - 10));

  // Never cover the watched box: its pixels must stay untouched.
  const overlaps =
    left < selection.x + selection.w &&
    left + rect.width > selection.x &&
    top < selection.y + selection.h &&
    top + rect.height > selection.y;
  if (overlaps) {
    top = selection.y - rect.height - gap;
    if (top < 10) {
      top = clamp(selection.y + selection.h + gap, 10, Math.max(10, window.innerHeight - rect.height - 10));
    }
  }

  bubble.style.left = `${left}px`;
  bubble.style.top = `${top}px`;

  if (first) requestAnimationFrame(() => bubble.classList.add("show"));
}

function showThinking(question) {
  streaming = null;
  bubble.classList.remove("hidden", "error");
  bubble.innerHTML = "";
  const q = document.createElement("div");
  q.className = "q";
  q.textContent = question;
  const dots = document.createElement("div");
  dots.className = "dots";
  dots.innerHTML = "<span></span><span></span><span></span>";
  bubble.append(q, dots);
  placeBubble();
}

function showAnswer(text) {
  if (streaming) {
    // The stream already built the bubble; just settle its final text.
    streaming.textContent = text;
    streaming.classList.remove("streaming");
    streaming = null;
    placeBubble();
    return;
  }
  bubble.classList.remove("hidden", "error");
  bubble.innerHTML = "";
  const label = document.createElement("div");
  label.className = "label";
  label.textContent = "Answer";
  const answer = document.createElement("div");
  answer.className = "a";
  answer.textContent = text;
  bubble.append(label, answer);
  placeBubble();
}

function onStream(text) {
  if (!text) return;
  if (!streaming) {
    bubble.classList.remove("hidden", "error");
    bubble.innerHTML = "";
    const label = document.createElement("div");
    label.className = "label";
    label.textContent = "Answer";
    const answer = document.createElement("div");
    answer.className = "a streaming";
    bubble.append(label, answer);
    streaming = answer;
  }
  streaming.textContent = text;
  placeBubble();
}

function showError(message) {
  streaming = null;
  bubble.classList.remove("hidden");
  bubble.classList.add("error");
  bubble.innerHTML = "";
  const label = document.createElement("div");
  label.className = "label";
  label.textContent = "Something went wrong";
  const text = document.createElement("div");
  text.className = "a";
  text.textContent = message;
  bubble.append(label, text);
  placeBubble();
}

/* ---------- pipeline stages ---------- */

function lockSelection() {
  if (!selection) return;
  paintSelection(selection);
  selEl.classList.remove("hidden");
  selEl.classList.add("locked", "flash");
  setTimeout(() => selEl.classList.remove("flash"), 1600);
}

function onSnip() {
  lockSelection();
  if (!snipBot) {
    snipBot = spawnBot(SNIP_ROBOT, snipTarget());
  }
  // Show something immediately: a slow vision model can take a while.
  showThinking("Reading the snip…");
}

function onRead(question) {
  if (snipBot) {
    sendAway(snipBot);
    snipBot = null;
  }

  if (!answerBot) {
    answerBot = spawnBot(ANSWER_ROBOT, answerTarget());
  }

  showThinking(question);
}

function onAnswer(text) {
  showAnswer(text);
  if (answerBot) {
    const target = answerTarget();
    moveBot(answerBot, target.x, target.y + 4, 400);
  }
}

function onError(message) {
  showError(message);
  sendAway(snipBot);
  sendAway(answerBot);
  snipBot = null;
  answerBot = null;
}

/* ---------- wiring ---------- */

listen("snip-start", () => {
  resetOverlay();
  dim.classList.remove("hidden");
  hint.classList.remove("hidden");
  document.body.style.cursor = "crosshair";
});

listen("watch", (event) => {
  const rect = event.payload?.rect;
  if (!rect) return;
  selection = { x: rect.x, y: rect.y, w: rect.w, h: rect.h };
  dim.classList.add("hidden");
  hint.classList.add("hidden");
  paintSelection(selection);
  selEl.classList.remove("hidden", "flash");
  selEl.classList.add("locked", "watching");
  document.body.style.cursor = "default";
});

listen("stage", (event) => {
  const { stage: kind, text } = event.payload ?? {};
  if (kind === "snip") onSnip();
  else if (kind === "read") onRead(text ?? "");
  else if (kind === "answer") onAnswer(text ?? "");
  else if (kind === "error") onError(text ?? "unknown error");
});

listen("stream", (event) => onStream(event.payload?.text ?? ""));

listen("reset", () => resetOverlay());

resetOverlay();
