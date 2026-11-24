"use strict";
const $ = (id) => document.getElementById(id);
let csrf = "",
  identity = null,
  bot = null,
  cursor = 0,
  active = null,
  oldest = "",
  polling = false,
  timer = null;
const turns = new Map();
function notice(message) {
  $("notice").textContent = message;
  $("notice").hidden = false;
  clearTimeout(timer);
  timer = setTimeout(() => ($("notice").hidden = true), 9000);
}
function activity(message) {
  const li = document.createElement("li");
  li.textContent =
    new Date().toLocaleTimeString([], {
      hour: "2-digit",
      minute: "2-digit",
      second: "2-digit",
    }) +
    " · " +
    message;
  $("activity").prepend(li);
  while ($("activity").children.length > 60) $("activity").lastChild.remove();
}
async function api(path, body) {
  const r = await fetch(path, {
    method: body === undefined ? "GET" : "POST",
    credentials: "same-origin",
    headers:
      body === undefined
        ? {}
        : { "content-type": "application/json", "x-buddy-csrf": csrf },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  if (!r.ok)
    throw new Error(
      r.status === 429
        ? "The server is busy. Please retry."
        : "Request failed. Check your session, provider configuration, and server status.",
    );
  return r.status === 204 ? null : r.json();
}
const call = (target, command) => api("/lab/api/action", { target, command });
function controls() {
  const ready = !!bot;
  $("message").disabled = !ready;
  $("send").disabled = !ready || !!active;
  $("cancel").hidden = !active && !window.BuddyVoice?.busy;
  $("claim").disabled = !ready;
  $("refresh-memory").disabled = !ready;
  $("older").disabled = !ready || !oldest;
  $("turn-status").textContent = active
    ? "Agent is working…"
    : ready
      ? "Enter to send · Shift + Enter for a new line"
      : "Connect a robot to chat";
}
function render() {
  const list = $("messages"),
    nearBottom = list.scrollHeight - list.scrollTop - list.clientHeight < 100;
  list.replaceChildren();
  for (const t of [...turns.values()].sort((a, b) =>
    a.id.localeCompare(b.id),
  )) {
    for (const [role, text] of [
      ["user", t.text],
      ["assistant", t.answer],
    ]) {
      if (!text && role === "user") continue;
      const bubble = document.createElement("article");
      bubble.className = "bubble " + role;
      const label = document.createElement("span");
      label.className = "who";
      label.textContent = role === "user" ? "You" : "Robot Buddy";
      const content = document.createElement("span");
      content.textContent =
        text ||
        (t.status === "running" || t.status === "queued"
          ? "Thinking…"
          : "No response text");
      bubble.append(label, content);
      if (
        role === "assistant" &&
        !["running", "queued", "completed"].includes(t.status)
      ) {
        const state = document.createElement("span");
        state.className = "state";
        state.textContent = t.status;
        bubble.append(state);
      }
      list.append(bubble);
    }
  }
  if (nearBottom) list.scrollTop = list.scrollHeight;
  controls();
}
async function history() {
  const snap = await call("conversation", { type: "snapshot" });
  turns.clear();
  for (const t of snap.turns) turns.set(t.id, t);
  cursor = snap.last_sequence || 0;
  oldest = snap.turns.at(-1)?.id || "";
  for (const t of snap.turns)
    if (t.speech_ready) window.BuddyVoice?.complete(t.id);
  active =
    snap.turns.find((t) => ["queued", "running"].includes(t.status))?.id ||
    null;
  render();
}
async function memory() {
  const facts = await call("memory", { type: "snapshot" });
  $("memories").replaceChildren();
  if (!facts.length) {
    const p = document.createElement("p");
    p.className = "hint";
    p.textContent =
      "No saved facts yet. Try “Remember my favorite color is blue.”";
    $("memories").append(p);
  }
  for (const f of facts) {
    const card = document.createElement("div");
    card.className = "memory-item";
    const key = document.createElement("strong"),
      value = document.createElement("p"),
      forget = document.createElement("button");
    key.textContent = f.key;
    value.textContent = f.value;
    forget.textContent = "Forget";
    forget.className = "quiet";
    forget.onclick = async () => {
      try {
        await call("memory", { type: "forget", key: f.key });
        await memory();
        activity("Forgot a saved fact");
      } catch (e) {
        notice(e.message);
      }
    };
    card.append(key, value, forget);
    $("memories").append(card);
  }
}
async function state() {
  const s = await api("/lab/api/state");
  csrf = s.csrf;
  window.BuddyVoice?.configure(s.speech_model);
  $("login-panel").hidden = true;
  $("workspace").hidden = false;
  $("logout").hidden = false;
  $("model").textContent =
    s.provider +
    " / " +
    s.model +
    (s.reasoning_effort ? " · " + s.reasoning_effort + " reasoning" : "");
  $("connection").textContent = "Lab connected";
  if (s.bot) {
    const changed = bot !== s.bot.bot_id;
    bot = s.bot.bot_id;
    $("bot-id").textContent = bot;
    $("owner").textContent = s.bot.owner
      ? "Registered"
      : "Device connected · unclaimed";
    if (!window.BuddyVoice?.busy)
      $("face-status").textContent = s.bot.owner
        ? "I'm here."
        : "Online · ready to register";
    $("claim-code").textContent = s.bot.confirm_code || "";
    $("confirm").hidden = !s.bot.confirm_code;
    $("claim").hidden = !!s.bot.owner;
    if (s.bot.owner) $("claim-qr").hidden = true;
    if (changed) {
      const pending = JSON.parse(
        sessionStorage.getItem("buddy-pending:" + bot) || "null",
      );
      if (pending) $("message").value = pending.text;
      await history();
      await memory();
      await settings();
      activity("Connected to durable robot actors");
    }
    if (!s.oidc_configured && !window.BuddyVoice?.busy)
      $("face-status").textContent =
        "Online · configure OIDC to test owner registration";
  }
  controls();
}
function event(e) {
  let t = turns.get(e.turn_id) || {
    id: e.turn_id,
    text: "",
    answer: "",
    status: "running",
  };
  if (e.kind === "turn.started") {
    t.text = e.payload.text || t.text;
    t.status = "running";
    active = t.id;
  }
  if (e.kind === "speech.ready" && e.payload.index === 0)
    window.BuddyVoice?.complete(e.turn_id);
  if (e.kind === "text.delta") t.answer += e.payload.text || "";
  if (e.kind.startsWith("turn.") && !["turn.started"].includes(e.kind)) {
    t.status = e.kind.slice(5);
    if (typeof e.payload.text === "string") t.answer = e.payload.text;
    if (active === t.id) active = null;
    activity("Turn " + t.status);
    if (["cancelled", "failed", "interrupted"].includes(t.status))
      window.BuddyVoice?.cancel(t.id);
  }
  if (e.kind.startsWith("tool.")) {
    activity(e.kind.replace(".", " · ") + " · " + e.payload.name);
    if (e.kind === "tool.completed") {
      memory().catch((e) => notice(e.message));
      settings().catch((e) => notice(e.message));
    }
  }
  turns.set(t.id, t);
  cursor = e.sequence;
  while (turns.size > 200) turns.delete([...turns.keys()].sort()[0]);
}
async function poll() {
  if (polling || !bot || document.hidden) return;
  polling = true;
  try {
    for (let i = 0; i < 8; i++) {
      const events = await call("conversation", {
        type: "replay",
        after: cursor,
      });
      if (!events.length) break;
      for (const e of events) if (e.sequence > cursor) event(e);
    }
    render();
    $("connection").textContent = "Connected · durable replay";
  } catch (e) {
    window.BuddyVoice?.disconnected();
    $("connection").textContent = "Reconnecting…";
  } finally {
    polling = false;
  }
}
$("login-form").onsubmit = async (e) => {
  e.preventDefault();
  try {
    const token = $("operator-token").value;
    await api("/lab/api/login", { token });
    $("operator-token").value = "";
    await state();
    activity("Operator authenticated");
  } catch (e) {
    notice(e.message);
  }
};
$("logout").onclick = async () => {
  window.BuddyVoice?.stop();
  try {
    await api("/lab/api/logout", {});
  } finally {
    identity = null;
    location.reload();
  }
};
$("identity-file").onchange = async (e) => {
  try {
    const file = e.target.files[0];
    if (!file) return;
    if (file.size > 8192) throw new Error("Identity file is too large.");
    const value = JSON.parse(await file.text());
    if (!value.certificate || !value.private_key)
      throw new Error("Choose a provisioned device identity JSON file.");
    identity = value;
    $("identity-status").textContent =
      "Identity loaded in memory · " + value.certificate.device_id;
    $("enroll").disabled = false;
  } catch (e) {
    identity = null;
    $("enroll").disabled = true;
    notice(e.message);
  }
};
function bytes(s) {
  return Uint8Array.from(atob(s.replace(/-/g, "+").replace(/_/g, "/")), (c) =>
    c.charCodeAt(0),
  );
}
function base64url(a) {
  let s = "";
  for (const b of new Uint8Array(a)) s += String.fromCharCode(b);
  return btoa(s).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}
$("enroll").onclick = async () => {
  if (!identity) return;
  $("enroll").disabled = true;
  try {
    const seed = bytes(identity.private_key);
    if (seed.length !== 32) throw new Error("Invalid device private key.");
    const prefix = Uint8Array.from([
        48, 46, 2, 1, 0, 48, 5, 6, 3, 43, 101, 112, 4, 34, 4, 32,
      ]),
      pkcs8 = new Uint8Array(48);
    pkcs8.set(prefix);
    pkcs8.set(seed, 16);
    const key = await crypto.subtle.importKey(
      "pkcs8",
      pkcs8,
      { name: "Ed25519" },
      false,
      ["sign"],
    );
    const response = await api("/v1/challenge", identity.certificate);
    const proof = base64url(
      await crypto.subtle.sign(
        "Ed25519",
        key,
        new TextEncoder().encode(response.challenge),
      ),
    );
    const result = await api("/lab/api/enroll", {
      certificate: identity.certificate,
      challenge: response.challenge,
      proof,
    });
    activity(
      bot === result.bot_id
        ? "Reconnected to the same robot"
        : "First-boot enrollment complete",
    );
    await state();
    $("enroll").textContent = "Reconnect same robot";
  } catch (e) {
    notice(e.message + " Use a current browser on HTTPS or localhost.");
  } finally {
    $("enroll").disabled = false;
  }
};
$("claim").onclick = async () => {
  try {
    const c = await call("bot", { type: "begin_claim" });
    $("qr").src = "data:image/svg+xml;base64," + btoa(c.qr);
    $("claim-link").href = c.url;
    $("claim-qr").hidden = false;
    activity("Registration QR ready · open sign-in");
  } catch (e) {
    notice(e.message);
  }
};
$("confirm").onclick = async () => {
  try {
    await call("bot", {
      type: "confirm_claim",
      code: $("claim-code").textContent,
    });
    await state();
    activity("Ownership confirmed on simulated robot");
  } catch (e) {
    notice(e.message);
  }
};
$("chat-form").onsubmit = async (e) => {
  e.preventDefault();
  if (!bot || active) return;
  const text = $("message").value.trim();
  if (!text) return;
  if (window.BuddyVoice?.enabled) {
    window.BuddyVoice.stop();
    await window.BuddyVoice.prepare();
  }
  const key = "buddy-pending:" + bot;
  let pending;
  try {
    pending = JSON.parse(sessionStorage.getItem(key) || "null");
    if (pending && pending.text !== text)
      throw new Error(
        "An earlier send is unconfirmed. Retry its original message before sending a new one.",
      );
    pending ||= { request_id: crypto.randomUUID(), text };
    sessionStorage.setItem(key, JSON.stringify(pending));
    $("send").disabled = true;
    const result = await call("conversation", { type: "submit", ...pending });
    sessionStorage.removeItem(key);
    if (!result.duplicate) window.BuddyVoice?.watch(result.turn_id);
    turns.set(result.turn_id, {
      id: result.turn_id,
      text,
      answer: turns.get(result.turn_id)?.answer || "",
      status: "queued",
    });
    active = result.turn_id;
    $("message").value = "";
    await history();
    if (turns.get(result.turn_id)?.status === "completed")
      window.BuddyVoice?.complete(result.turn_id);
    await poll();
  } catch (e) {
    notice(e.message + " Retry Send to use the same request ID.");
    controls();
  }
};
$("message").onkeydown = (e) => {
  if (e.key === "Enter" && !e.shiftKey) {
    e.preventDefault();
    $("chat-form").requestSubmit();
  }
};
$("cancel").onclick = async () => {
  window.BuddyVoice?.stop();
  if (!active) return;
  try {
    await call("conversation", { type: "cancel", turn_id: active });
    await poll();
  } catch (e) {
    notice(e.message);
  }
};
$("refresh-memory").onclick = () => memory().catch((e) => notice(e.message));
$("older").onclick = async () => {
  try {
    const page = await call("conversation", {
      type: "history",
      before: oldest,
    });
    for (const t of page) turns.set(t.id, t);
    oldest = page.at(-1)?.id || "";
    render();
  } catch (e) {
    notice(e.message);
  }
};
for (const b of document.querySelectorAll("[data-prompt]"))
  b.onclick = () => {
    $("message").value = b.dataset.prompt;
    $("message").focus();
  };
state().catch(() => {});
setInterval(() => {
  if (active) poll();
}, 200);
setInterval(poll, 1000);
setInterval(() => {
  if (csrf && !document.hidden)
    state().catch(() => {
      window.BuddyVoice?.disconnected();
      $("connection").textContent = "Session unavailable";
    });
}, 5000);

$("inspect-persona").onclick = async () => {
  if (!bot) return notice("Connect a robot first.");
  try {
    const [current, versions] = await Promise.all([
      call("conversation", { type: "get_system_prompt" }),
      call("conversation", { type: "system_prompt_history" }),
    ]);
    $("persona-version").textContent =
      "Current revision " +
      current.revision +
      " · stored in this robot’s Conversation actor";
    $("persona-revisions").replaceChildren();
    for (const v of versions) {
      const option = document.createElement("option");
      option.value = String(v.revision);
      option.textContent = "Revision " + v.revision;
      $("persona-revisions").append(option);
    }
    const show = () => {
      const v = versions.find(
        (v) => String(v.revision) === $("persona-revisions").value,
      );
      $("persona-text").value = v?.prompt || current.prompt;
      $("persona-reason").textContent = v?.reason || "";
    };
    $("persona-revisions").onchange = show;
    show();
    $("persona-dialog").showModal();
  } catch (e) {
    notice(e.message);
  }
};

async function settings() {
  if (!bot) return;
  const [v, c, profiles, prompt] = await Promise.all([
    call("conversation", { type: "get_voice_config" }),
    call("conversation", { type: "get_context_config" }),
    call("conversation", { type: "list_personas" }),
    call("conversation", { type: "get_system_prompt" }),
  ]);
  $("active-prompt").textContent = prompt.prompt;
  $("active-prompt-revision").textContent =
    "Prompt revision " +
    prompt.revision +
    " · Voice revision " +
    v.revision +
    " · Location revision " +
    c.revision;
  $("active-context").textContent = JSON.stringify(c, null, 2);
  $("persona-choice").replaceChildren();
  for (const profile of profiles) {
    const o = document.createElement("option");
    o.value = profile.name;
    o.textContent = profile.name + " · v" + profile.latest_version;
    $("persona-choice").append(o);
  }
  await personaVersions();
  $("voice-settings").textContent = JSON.stringify(v.config, null, 2);
  $("saved-voice").textContent = v.name + " · revision " + v.revision;
  $("voice-direction").textContent = v.config.delivery;
  $("voice-choice").replaceChildren();
  for (const choice of v.choices) {
    const o = document.createElement("option");
    o.value = choice.key;
    o.textContent = choice.name;
    o.selected = choice.key === v.config.voice;
    $("voice-choice").append(o);
  }
  $("robot-context").textContent =
    (c.config.location || "Location not set") +
    " · " +
    c.config.timezone +
    " · revision " +
    c.revision;
}
$("request-voice").onclick = () => {
  if (!bot) return notice("Connect a robot first.");
  $("message").value =
    "Please switch your voice to " +
    $("voice-choice").selectedOptions[0].textContent +
    " and keep your current delivery style.";
  $("message").focus();
};

$("request-persona").onclick = () => {
  if (!bot) return notice("Connect a robot first.");
  $("message").value =
    "Please switch to my saved persona " +
    $("persona-choice").value +
    " version " +
    $("persona-saved-version").value +
    ". Use its saved voice settings too.";
  $("message").focus();
};

async function personaVersions() {
  if (!$("persona-choice").value) return;
  const profile = await call("conversation", {
    type: "get_persona",
    name: $("persona-choice").value,
    version: 0,
  });
  $("persona-saved-version").replaceChildren();
  for (const v of profile.versions) {
    const o = document.createElement("option");
    o.value = v.version;
    o.textContent = "Version " + v.version + " · " + v.reason;
    $("persona-saved-version").append(o);
  }
}
$("persona-choice").onchange = () =>
  personaVersions().catch((e) => notice(e.message));

$("refresh-settings").onclick = () =>
  settings().catch((e) => notice(e.message));
