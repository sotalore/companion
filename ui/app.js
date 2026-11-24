// The window. It asks the Rust side for the whole state (`get_state`), draws it, and draws it again
// whenever the Rust side says something changed (the `changed` event). Every action is a command.
// Text from files and the site is always set with textContent, never as HTML.

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);

// Makes an element: el("td", { class: "muted" }, "text", otherElement)
function el(tag, attrs = {}, ...children) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(attrs)) {
    if (key === "onclick") node.addEventListener("click", value);
    else node.setAttribute(key, value);
  }
  for (const child of children) {
    if (child != null) node.append(child);
  }
  return node;
}

function showError(message) {
  const box = $("error");
  box.textContent = message || "";
  box.hidden = !message;
}

// Runs a command, showing its error message if it fails.
async function run(command, args) {
  showError("");
  try {
    return await invoke(command, args);
  } catch (err) {
    showError(String(err));
    throw err;
  }
}

function reviewLink(url, text = "Review") {
  return el("button", { class: "link", onclick: () => run("open_url", { url }) }, text);
}

const CONNECTION_CLASS = { ok: "ok", checking: "", unchecked: "", offline: "warn", no_token: "warn" };

let settingsDrawn = false;

function draw(state) {
  $("version").textContent = "v" + state.version;

  // Site and token
  const conn = state.connection;
  const status = $("connection");
  status.className = "status " + (CONNECTION_CLASS[conn.state] ?? "bad");
  status.textContent = conn.message + (conn.scopes.length ? ` (scopes: ${conn.scopes.join(", ")})` : "");
  $("token").placeholder = state.has_token ? "Saved. Paste a new one to replace it." : "slt_...";
  $("clear-token").disabled = !state.has_token;

  // Settings: fill the form once, so a redraw doesn't wipe what's being typed.
  if (!settingsDrawn) {
    $("lua-folder").value = state.settings.lua_folder ?? "";
    $("shot-folder").value = state.settings.screenshot_folder ?? "";
    $("site-url").value = state.settings.site_url;
    $("auto-upload").checked = state.settings.auto_upload;
    $("talk-key").value = state.settings.talk_key;
    $("whisper-model").value = state.settings.whisper_model ?? "";
    const presets = (list, input) =>
      list
        .filter((f) => f.exists)
        .map((f) => el("button", { type: "button", onclick: () => ($(input).value = f.path) }, f.label));
    $("lua-presets").replaceChildren(...presets(state.lua_folders, "lua-folder"));
    $("shot-presets").replaceChildren(...presets(state.screenshot_folders, "shot-folder"));
    settingsDrawn = true;
  }
  $("client-mismatch").hidden = !state.client_mismatch;
  $("client-mismatch").textContent = state.client_mismatch ?? "";
  const watching = [];
  watching.push(state.watching_lua
    ? `Watching ${state.watching_lua} for scene changes` + (state.auto_uploading ? " and new exports." : ".")
    : "Not watching the Lua folder: choose it above.");
  if (state.watching_lua && !state.auto_uploading) watching.push("Exports aren't uploaded automatically: " + whyNoAutoUpload(state));
  watching.push(state.watching_shots
    ? `Watching ${state.watching_shots} for screenshots.`
    : "Not watching for screenshots: choose the screenshot folder above.");
  $("watching").textContent = watching.join(" ");

  // Screenshots need `read` (to find scenes) and `images` (to send); say so before a send fails.
  const missing = conn.state === "ok" ? ["read", "images"].filter((s) => !conn.scopes.includes(s)) : [];
  $("shot-scopes").hidden = missing.length === 0;
  $("shot-scopes").textContent = missing.length
    ? `This token doesn't have ${missing.map((s) => "`" + s + "`").join(" or ")}, which screenshots need. ` +
      "Make a new token at ADM → AI Access with upload, read and images, and paste it under Site: it replaces the saved one."
    : "";

  drawSkills(state.skills);
  drawScene(state.current_scene);
  drawShots(state.screenshots, state);
  drawNotes(state);

  // Files
  const rows = state.files.map((f) => {
    let uploaded;
    if (f.uploaded_at) {
      uploaded = el("span", {}, f.uploaded_at + " ", f.review_url ? reviewLink(f.review_url) : null);
    } else if (f.previous_upload_at) {
      uploaded = el("span", { class: "muted" }, `changed since ${f.previous_upload_at}`);
    } else {
      uploaded = el("span", { class: "muted" }, f.uploadable ? "not yet" : "not uploaded");
    }
    const button = f.uploadable
      ? el("button", { onclick: () => run("upload_file", { name: f.name }) }, "Upload")
      : null;
    return el("tr", {}, el("td", {}, f.name), el("td", {}, f.label), el("td", { class: "muted" }, f.modified),
      el("td", {}, uploaded), el("td", {}, button));
  });
  if (rows.length === 0) {
    rows.push(el("tr", {}, el("td", { colspan: "5", class: "muted" }, "No sotalore-*.json files in the folder.")));
  }
  $("files").replaceChildren(...rows);

  // Activity
  const lines = state.activity.map((a) =>
    el("li", {},
      el("span", { class: "when" }, a.at),
      el("span", { class: "tag " + a.status }, a.status),
      a.file, a.label ? ` · ${a.label}` : "",
      el("span", { class: "msg" }, a.message, a.review_url ? " " : null, a.review_url ? reviewLink(a.review_url, "Review it on the site") : null),
    ));
  if (lines.length === 0) lines.push(el("li", { class: "muted" }, "Nothing uploaded since the app started."));
  $("activity").replaceChildren(...lines);
}

function whyNoAutoUpload(state) {
  if (!state.settings.auto_upload) return "automatic uploads are off; use the Upload buttons.";
  if (!state.has_token) return "add an API token.";
  if (state.connection.state === "bad_token") return "the site refused the token.";
  return "the token doesn't have the upload scope.";
}

// Seconds since 1970 → local time, e.g. "Nov 23, 11:58".
function localTime(seconds) {
  return new Date(seconds * 1000).toLocaleString(undefined, { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" });
}

function sceneText(scene) {
  let text = `${scene.name ?? scene.raw_name} (${scene.raw_name})`;
  if (scene.dungeon_owner) text += `, in ${scene.dungeon_owner}'s ${scene.dungeon_name ? `"${scene.dungeon_name}"` : "dungeon"}`;
  if (scene.character) text += ` · ${scene.character}`;
  return text;
}

function drawScene(scene) {
  const line = $("current-scene");
  if (!scene) {
    line.className = "status warn";
    line.textContent = "No scene yet: sl-admin 0.16.0 or later writes it to the Lua folder when you change scenes.";
  } else if (scene.state === "inScene") {
    line.className = "status ok";
    line.textContent = `Now in ${sceneText(scene)}, since ${localTime(scene.entered_at)}${scene.exact ? "" : " or earlier"}.`;
  } else {
    line.className = "status";
    line.textContent = (scene.state === "loading" ? "Loading a scene" : "Logged out") + `, since ${localTime(scene.entered_at)}.`;
  }
}

// The skills check: a status line, then each kind of difference, then the issue text to copy.
let skillsDrawnFor = null; // redraw the report only when it changes, so a copy in progress isn't wiped

function drawSkills(check) {
  const status = $("skills-status");
  const r = check.report;
  status.className = "status " + ({ done: r?.in_step ? "ok" : "warn", failed: "bad" }[check.status] ?? "");
  status.textContent = check.message || "Not checked yet.";
  $("check-skills").disabled = check.status === "checking";

  const key = r ? r.checked_at + r.file : null;
  if (key === skillsDrawnFor) return;
  skillsDrawnFor = key;
  const box = $("skills-report");
  if (!r) { box.replaceChildren(); return; }
  const parts = [el("p", { class: "hint" },
    `${r.file} (sl-admin ${r.plugin_version ?? "?"}, game API ${r.api_version ?? "?"}): ${r.game_count} skills in the game, ` +
    `${r.site_count} on the site. Checked ${r.checked_at}.`)];
  const sections = [
    ["New in the game", r.added], ["Changed", r.changed], ["Moved school or category", r.moved],
    ["Gone from the game", r.removed], ["On the site with no game id", r.unlinked],
  ];
  for (const [title, lines] of sections) {
    if (!lines.length) continue;
    parts.push(el("details", { open: "" },
      el("summary", {}, `${title} (${lines.length})`),
      el("ul", { class: "skills" }, ...lines.map((l) =>
        el("li", {}, el("strong", {}, l.name), l.game_id != null ? el("span", { class: "muted" }, ` #${l.game_id}`) : null,
          el("span", { class: "muted" }, ` · ${l.place}`), el("div", {}, l.details.join("; ")))))));
  }
  if (r.issue_text) {
    const text = el("textarea", { readonly: "", rows: "8" });
    text.value = r.issue_text;
    const copy = el("button", { type: "button", onclick: async () => {
      try {
        await navigator.clipboard.writeText(r.issue_text);
        copy.textContent = "Copied";
      } catch {
        text.select(); // the clipboard isn't available: select it for Ctrl+C
        copy.textContent = "Press Ctrl+C";
      }
    } }, "Copy issue text");
    parts.push(el("label", {}, "For an issue in sotalore/sotalore:"), text, el("div", { class: "row" }, copy));
  }
  box.replaceChildren(...parts);
}

// The screenshot cards, keyed by path, so a redraw keeps each card's image and any open scene search.
const cards = new Map();
const OPEN = ["waiting", "failed", "sending", "retrying"];

function drawShots(shots, state) {
  // Everything still open, and the last few finished.
  const shown = [...shots.filter((s) => OPEN.includes(s.status)), ...shots.filter((s) => !OPEN.includes(s.status)).slice(0, 5)];
  const keep = new Set(shown.map((s) => s.path));
  for (const [path, card] of cards) {
    if (!keep.has(path)) { card.root.remove(); cards.delete(path); }
  }
  const list = $("shots");
  shown.forEach((shot, i) => {
    let card = cards.get(shot.path);
    if (!card) { card = makeCard(shot); cards.set(shot.path, card); }
    updateCard(card, shot, state);
    if (list.children[i] !== card.root) list.insertBefore(card.root, list.children[i] ?? null);
  });
  if (shown.length === 0) {
    if (!$("no-shots")) list.append(el("p", { id: "no-shots", class: "muted" }, "No new screenshots. F11 in game takes one (F10 hides the interface first)."));
  } else {
    $("no-shots")?.remove();
  }
}

function makeCard(shot) {
  const img = el("img", { src: window.__TAURI__.core.convertFileSrc(shot.path), alt: shot.file, loading: "lazy" });
  const info = el("div", { class: "info" });
  const actions = el("div", { class: "row" });
  const chooser = el("div", { class: "chooser", hidden: "" });
  const root = el("div", { class: "shot" }, img, el("div", { class: "body" }, info, actions, chooser));
  return { root, info, actions, chooser, img };
}

function updateCard(card, shot, state) {
  card.root.className = "shot " + shot.status;
  const taken = new Date(shot.taken_at).toLocaleString();
  const lines = [el("div", {}, el("span", { class: "tag " + shot.status }, shot.status), shot.file, el("span", { class: "muted" }, " · " + taken))];
  lines.push(el("div", {}, "Scene: ", shot.scene ? sceneText(shot.scene) : el("span", { class: "muted" }, "none")));
  if (shot.scene_note) lines.push(el("div", { class: "muted" }, shot.scene_note));
  if (shot.site_scene) {
    const s = shot.site_scene;
    const where = [s.region, s.level].filter(Boolean).join(", ");
    lines.push(el("div", {}, "On the site: ", s.url ? reviewLink(s.url, s.name) : s.name, where ? el("span", { class: "muted" }, ` (${where})`) : null));
  } else if (shot.site_note) {
    lines.push(el("div", { class: "warn" }, shot.site_note));
  }
  if (shot.message) lines.push(el("div", { class: "muted" }, shot.message, shot.image_url ? " " : null, shot.image_url ? reviewLink(shot.image_url, "See it") : null));
  card.info.replaceChildren(...lines);

  const open = ["waiting", "failed"].includes(shot.status);
  const buttons = [];
  if (open && shot.site_scene) {
    buttons.push(el("button", { class: "send", onclick: () => run("send_screenshot", { path: shot.path, scene: shot.site_scene }) }, `Send to ${shot.site_scene.name}`));
  }
  if (open) {
    buttons.push(el("button", { onclick: () => toggleChooser(card, shot) }, shot.site_scene ? "Another scene..." : "Choose a scene..."));
    buttons.push(el("button", { onclick: () => run("skip_screenshot", { path: shot.path }) }, "Skip"));
  }
  card.actions.replaceChildren(...buttons);
  if (!open) card.chooser.hidden = true;
}

// A search box for choosing the site scene by name; each result is a Send button for that scene.
function toggleChooser(card, shot) {
  const box = card.chooser;
  if (!box.hidden) { box.hidden = true; return; }
  const input = el("input", { type: "text", placeholder: "Scene name...", value: shot.scene?.name ?? "" });
  const results = el("div", { class: "results" });
  const search = async () => {
    results.replaceChildren(el("span", { class: "muted" }, "Searching..."));
    let scenes;
    try {
      scenes = await run("search_scenes", { query: input.value });
    } catch (err) {
      results.replaceChildren(el("span", { class: "error" }, String(err)));
      return;
    }
    results.replaceChildren(
      ...(scenes.length ? scenes.map((s) => {
        const where = [s.region, s.level].filter(Boolean).join(", ");
        return el("button", { onclick: () => { box.hidden = true; run("send_screenshot", { path: shot.path, scene: s }); } },
          `Send to ${s.name}${where ? ` (${where})` : ""}`);
      }) : [el("span", { class: "muted" }, "No scene with that in its name.")]),
    );
  };
  input.addEventListener("keydown", (e) => { if (e.key === "Enter") search(); });
  box.replaceChildren(el("div", { class: "row" }, input, el("button", { onclick: search }, "Search")), results);
  box.hidden = false;
  input.focus();
}

// Spoken notes: a card per note, keyed by id, so a redraw keeps the audio player, any open scene
// search, and a transcript being corrected.
const noteCards = new Map();
const BUSY = ["queued", "transcribing"];

function drawNotes(state) {
  const talk = state.talk;
  $("talk").className = "status " + (talk.recording ? "bad" : talk.key ? "ok" : "warn");
  $("talk").textContent = talk.recording ? "Recording... let go of the key to stop." : talk.message;

  const notes = state.notes;
  const todo = notes.filter((n) => ["recorded", "failed"].includes(n.status)).length;
  const busy = notes.some((n) => BUSY.includes(n.status));
  const button = $("transcribe");
  button.textContent = todo ? `Transcribe ${todo} note${todo === 1 ? "" : "s"}` : "Transcribe";
  button.disabled = busy || todo === 0 || !state.has_whisper;
  $("transcribe-status").textContent = !state.has_whisper
    ? "This build can't transcribe yet (it needs the whisper feature)."
    : busy ? "Transcribing..." : !state.settings.whisper_model ? "Choose the Whisper model file in the settings." : "";

  const keep = new Set(notes.map((n) => n.id));
  for (const [id, card] of noteCards) {
    if (!keep.has(id)) { card.root.remove(); noteCards.delete(id); }
  }
  const list = $("notes");
  notes.forEach((note, i) => {
    let card = noteCards.get(note.id);
    if (!card) { card = makeNoteCard(note); noteCards.set(note.id, card); }
    updateNoteCard(card, note);
    if (list.children[i] !== card.root) list.insertBefore(card.root, list.children[i] ?? null);
  });
  if (notes.length === 0) {
    if (!$("no-notes")) list.append(el("p", { id: "no-notes", class: "muted" }, "No notes yet."));
  } else {
    $("no-notes")?.remove();
  }
}

function makeNoteCard(note) {
  const audio = el("audio", { controls: "", preload: "none", src: window.__TAURI__.core.convertFileSrc(note.path) });
  const info = el("div", { class: "info" });
  const text = el("textarea", { rows: "3", placeholder: "Not transcribed yet." });
  const save = el("button", { type: "button", hidden: "" }, "Save text");
  const actions = el("div", { class: "row" });
  const chooser = el("div", { class: "chooser", hidden: "" });
  const card = { root: null, info, text, save, actions, chooser, dirty: false };
  text.addEventListener("input", () => { card.dirty = true; save.hidden = false; });
  save.addEventListener("click", async () => {
    await run("set_note_transcript", { id: note.id, text: text.value });
    card.dirty = false;
    save.hidden = true;
  });
  card.root = el("div", { class: "note" }, el("div", { class: "body" }, info, audio, text, el("div", { class: "row" }, save), actions, chooser));
  return card;
}

function updateNoteCard(card, note) {
  card.root.className = "note " + note.status;
  const when = new Date(note.recorded_at).toLocaleString();
  const lines = [el("div", {}, el("span", { class: "tag " + note.status }, note.status), when,
    el("span", { class: "muted" }, ` · ${note.seconds.toFixed(1)} s`))];
  lines.push(el("div", {}, "Scene: ", note.scene ? sceneText(note.scene) : el("span", { class: "muted" }, "none")));
  if (note.scene_note) lines.push(el("div", { class: "muted" }, note.scene_note));
  if (note.site_scene) {
    const s = note.site_scene;
    const where = [s.region, s.level].filter(Boolean).join(", ");
    lines.push(el("div", {}, "On the site: ", s.url ? reviewLink(s.url, s.name) : s.name, where ? el("span", { class: "muted" }, ` (${where})`) : null));
  } else if (note.site_note) {
    lines.push(el("div", { class: "warn" }, note.site_note));
  }
  if (note.message) lines.push(el("div", { class: "muted" }, note.message));
  card.info.replaceChildren(...lines);

  // Don't overwrite a correction being typed.
  if (!card.dirty && document.activeElement !== card.text) card.text.value = note.transcript ?? "";
  card.text.disabled = BUSY.includes(note.status);

  const buttons = [el("button", { onclick: () => toggleNoteChooser(card, note) }, note.site_scene ? "Another scene..." : "Choose a scene...")];
  if (!BUSY.includes(note.status)) {
    buttons.push(el("button", { onclick: () => {
      if (confirm("Delete this note and its recording?")) run("delete_note", { id: note.id });
    } }, "Delete"));
  }
  card.actions.replaceChildren(...buttons);
}

// Choosing a note's scene by name: each result sets it (nothing is sent).
function toggleNoteChooser(card, note) {
  const box = card.chooser;
  if (!box.hidden) { box.hidden = true; return; }
  const input = el("input", { type: "text", placeholder: "Scene name...", value: note.scene?.name ?? "" });
  const results = el("div", { class: "results" });
  const search = async () => {
    results.replaceChildren(el("span", { class: "muted" }, "Searching..."));
    let scenes;
    try {
      scenes = await run("search_scenes", { query: input.value });
    } catch (err) {
      results.replaceChildren(el("span", { class: "error" }, String(err)));
      return;
    }
    results.replaceChildren(
      ...(scenes.length ? scenes.map((s) => {
        const where = [s.region, s.level].filter(Boolean).join(", ");
        return el("button", { onclick: () => { box.hidden = true; run("set_note_scene", { id: note.id, scene: s }); } },
          `${s.name}${where ? ` (${where})` : ""}`);
      }) : [el("span", { class: "muted" }, "No scene with that in its name.")]),
    );
  };
  input.addEventListener("keydown", (e) => { if (e.key === "Enter") search(); });
  box.replaceChildren(el("div", { class: "row" }, input, el("button", { onclick: search }, "Search")), results);
  box.hidden = false;
  input.focus();
}

async function refresh() {
  draw(await invoke("get_state"));
}

$("token-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  await run("save_token", { token: $("token").value });
  $("token").value = "";
});
$("clear-token").addEventListener("click", () => run("clear_token"));
$("recheck").addEventListener("click", () => run("check_connection"));
$("check-skills").addEventListener("click", () => run("check_skills"));
$("transcribe").addEventListener("click", () => run("transcribe_notes"));

$("settings-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  await run("save_settings", {
    luaFolder: $("lua-folder").value,
    screenshotFolder: $("shot-folder").value,
    siteUrl: $("site-url").value,
    autoUpload: $("auto-upload").checked,
    talkKey: $("talk-key").value,
    whisperModel: $("whisper-model").value,
  });
  settingsDrawn = false; // show the saved values as the app tidied them
  await refresh();
});

listen("changed", refresh);
refresh();
