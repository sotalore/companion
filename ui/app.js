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
    $("site-url").value = state.settings.site_url;
    $("auto-upload").checked = state.settings.auto_upload;
    const presets = $("lua-presets");
    presets.replaceChildren(
      ...state.lua_folders
        .filter((f) => f.exists)
        .map((f) => el("button", { type: "button", onclick: () => ($("lua-folder").value = f.path) }, f.label)),
    );
    settingsDrawn = true;
  }
  $("watching").textContent = state.watching
    ? `Watching ${state.watching} for new exports.`
    : "Not watching: " + whyNotWatching(state);

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

function whyNotWatching(state) {
  if (!state.settings.lua_folder) return "choose the game's Lua folder.";
  if (!state.settings.auto_upload) return "automatic uploads are off. Use the Upload buttons.";
  if (!state.has_token) return "add an API token.";
  if (["bad_token", "no_upload_scope"].includes(state.connection.state)) return "the token can't upload.";
  return "the Lua folder can't be watched.";
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

$("settings-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  await run("save_settings", {
    luaFolder: $("lua-folder").value,
    siteUrl: $("site-url").value,
    autoUpload: $("auto-upload").checked,
  });
  settingsDrawn = false; // show the saved values as the app tidied them
  await refresh();
});

listen("changed", refresh);
refresh();
