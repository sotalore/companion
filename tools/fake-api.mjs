// A stand-in for the site's API, for trying the companion without a real token or touching the site.
// It answers GET /api/v1/me, the three upload endpoints, GET /scenes and POST /scenes/:id/images the
// way sotalore/docs/api.md says, and GET /skills (see siteSkills below). It keeps nothing on disk
// (images are counted, not stored).
//
//   npm run fake-api                    # http://localhost:4567, token slt_fake (upload, read, images)
//   FAIL_FIRST=2 npm run fake-api       # the first 2 uploads or images get a 503, to watch the retries
//   SCOPES=upload,images npm run fake-api   # a token without read (or leave out any scope)
//
// In the companion, set the site to http://localhost:4567 and the token to slt_fake.
// Its scenes: Owl's Head (game name OwlsHead_01), Novia (Novia), and West Ravenswood, which has no
// game name yet, so a screenshot taken there has to be given its scene by name.

import { createServer } from "node:http";
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";

const PORT = Number(process.env.PORT ?? 4567);
const TOKEN = process.env.TOKEN ?? "slt_fake";
const SCOPES = (process.env.SCOPES ?? "upload,read,images").split(",");
let failFirst = Number(process.env.FAIL_FIRST ?? 0);

const base = `http://localhost:${PORT}`;
const SCENES = [
  { id: 41, name: "Owl's Head", game_name: "OwlsHead_01", region: "Novia", scene_type: "town", level: "Tier 1", level_plus: false, parent: null },
  { id: 7, name: "Novia", game_name: "Novia", region: "Novia", scene_type: "overworld", level: null, level_plus: false, parent: null },
  { id: 12, name: "West Ravenswood", game_name: null, region: "Novia", scene_type: "wilderness", level: "Tier 2", level_plus: false, parent: null },
].map((s) => ({ ...s, images: 0, url: `${base}/scenes/${s.id}` }));
const sceneImages = new Map(); // "sceneId:sha256" -> the image made

const ENDPOINTS = {
  "/api/v1/recipe_imports": { type: "recipes", count: (j) => ({ entries: j.recipes?.length ?? 0, full_export: j.scope === "all", scope: j.scope ?? null }) },
  "/api/v1/item_catalog_imports": { type: "items", count: (j) => ({ items: j.items?.length ?? 0, plugin_version: j.pluginVersion }) },
  "/api/v1/finds_imports": { type: "finds", count: (j) => ({ scenes: j.scenes?.length ?? 0, plugin_version: j.pluginVersion }) },
};
const REVIEW = { recipes: "recipe_imports", items: "item_catalog_imports", finds: "finds_imports" };

const seen = new Map(); // sha256 of the file -> the import made for it
let nextId = 1;

function send(res, status, body) {
  res.writeHead(status, { "Content-Type": "application/json" });
  res.end(JSON.stringify(body));
  console.log(`  -> ${status} ${JSON.stringify(body)}`);
}

// Pulls a field out of a multipart/form-data body: { filename, content } (content is a Buffer).
function formField(contentType, body, name = "file") {
  const boundary = /boundary=(?:"([^"]+)"|([^;]+))/.exec(contentType ?? "");
  if (!boundary) return null;
  const marker = "--" + (boundary[1] ?? boundary[2]);
  const text = body.toString("latin1"); // byte for byte, so the hash matches the file's
  for (const part of text.split(marker)) {
    const split = part.indexOf("\r\n\r\n");
    if (split < 0) continue;
    const headers = part.slice(0, split);
    if (!new RegExp(`name="${name}"`).test(headers)) continue;
    const filename = /filename="([^"]*)"/.exec(headers)?.[1] ?? "upload";
    const content = Buffer.from(part.slice(split + 4).replace(/\r\n$/, ""), "latin1");
    return { filename, content };
  }
  return null;
}
const fileField = (contentType, body) => formField(contentType, body, "file");

// The image's type from its bytes, as the site does: PNG, JPEG or WebP, or null.
function imageType(b) {
  if (b.subarray(0, 8).equals(Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]))) return "image/png";
  if (b[0] === 0xff && b[1] === 0xd8) return "image/jpeg";
  if (b.subarray(0, 4).toString("latin1") === "RIFF" && b.subarray(8, 12).toString("latin1") === "WEBP") return "image/webp";
  return null;
}

// GET /api/v1/skills: the site's skills, built from a game-skills file the way `rails skills:sync`
// would (SKILLS_FILE, by default the regular client's), then put out of step with the game on
// purpose unless SKILLS_DRIFT=0: one skill renamed, one with another XP factor, one in another
// school, one missing (so "new in the game"), one the game no longer has, and one with no game id.
function siteSkills() {
  const file = process.env.SKILLS_FILE ??
    `${process.env.APPDATA}/Catnip Games/Shroud of the Avatar/Lua/sotalore-game-skills.json`;
  let game = [];
  try {
    game = JSON.parse(readFileSync(file, "utf8")).skills;
  } catch {
    console.log(`  (no game-skills file at ${file}: the site has no skills)`);
  }
  const SCHOOLS = { polearms: "polearm", moon: "lunar" };
  const param = (s) => s.toLowerCase().replace(/[^a-z0-9]+/g, "-");
  let skills = game.map((g, i) => {
    const category = g.subcategory.toLowerCase();
    const school = SCHOOLS[g.school.toLowerCase()] ?? g.school.toLowerCase();
    return {
      id: i + 1, key: `${category}~${param(school)}~${param(g.name)}`, name: g.name,
      activity: ["combat", "magic", "strategy"].includes(category) ? "adventuring" : "crafting",
      category, school, xp_factor: Number(g.xpMultiplier), game_id: g.id, game_key: g.key,
    };
  });
  if (process.env.SKILLS_DRIFT !== "0" && skills.length > 5) {
    skills[0].name = skills[0].name + " (old name)";
    skills[1].xp_factor = skills[1].xp_factor * 2;
    skills[2].school = "an-old-school";
    skills.splice(3, 1);
    skills.push({ id: 9001, key: "magic~life~life-warding", name: "Life Warding", activity: "adventuring",
      category: "magic", school: "life", xp_factor: 1, game_id: 3757, game_key: "LifeWarding" });
    skills.push({ id: 9002, key: "combat~blades~hand-made", name: "Hand Made", activity: "adventuring",
      category: "combat", school: "blades", xp_factor: 1, game_id: null, game_key: null });
  }
  return { skills, game_schools: SCHOOLS };
}

// GET /api/v1/scenes and POST /api/v1/scenes/:id/images. Returns true if it answered.
function scenesApi(req, res, body) {
  const url = new URL(req.url, base);
  if (req.method === "GET" && url.pathname === "/api/v1/skills") {
    if (!SCOPES.includes("read")) return send(res, 403, { error: "This token doesn't have the read scope" }), true;
    const body = siteSkills();
    res.writeHead(200, { "Content-Type": "application/json" });
    res.end(JSON.stringify(body));
    console.log(`  -> 200 (${body.skills.length} skills)`);
    return true;
  }
  if (req.method === "GET" && url.pathname === "/api/v1/scenes") {
    if (!SCOPES.includes("read")) return send(res, 403, { error: "This token doesn't have the read scope" }), true;
    const p = url.searchParams;
    const list = SCENES.filter((s) =>
      (!p.has("game_name") || s.game_name === p.get("game_name")) &&
      (!p.has("name") || s.name.toLowerCase() === p.get("name").toLowerCase()) &&
      (!p.has("q") || s.name.toLowerCase().includes(p.get("q").toLowerCase())));
    send(res, 200, { scenes: list, page: { number: 1, per: Number(p.get("per") ?? 25), total_count: list.length, total_pages: 1 } });
    return true;
  }
  const match = /^\/api\/v1\/scenes\/(\d+)\/images$/.exec(url.pathname);
  if (req.method !== "POST" || !match) return false;
  if (!SCOPES.includes("images")) return send(res, 403, { error: "This token doesn't have the images scope" }), true;
  const scene = SCENES.find((s) => s.id === Number(match[1]));
  if (!scene) return send(res, 404, { error: "Not found" }), true;
  if (failFirst > 0) {
    failFirst -= 1;
    return send(res, 503, { error: "Service unavailable (fake)" }), true;
  }
  const file = fileField(req.headers["content-type"], body);
  if (!file) return send(res, 400, { error: "No file field" }), true;
  if (file.content.length > 20 * 1024 * 1024) return send(res, 413, { error: "Over 20 MB" }), true;
  const type = imageType(file.content);
  if (!type) return send(res, 422, { error: "That isn't a PNG, JPEG or WebP image" }), true;
  const takenAt = formField(req.headers["content-type"], body, "taken_at")?.content.toString("utf8");
  if (takenAt && Number.isNaN(Date.parse(takenAt))) return send(res, 422, { error: "taken_at isn't a time" }), true;
  const key = `${scene.id}:${createHash("sha256").update(file.content).digest("hex")}`;
  if (sceneImages.has(key)) return send(res, 200, sceneImages.get(key)), true;
  const id = nextId++;
  const image = {
    id, scene: { id: scene.id, name: scene.name, url: scene.url },
    url: `${base}/images/${id}`, preview_url: `${base}/images/${id}/preview`,
    content_type: type, byte_size: file.content.length, created_at: new Date().toISOString(),
  };
  console.log(`  ${file.filename}, ${file.content.length} bytes, taken_at ${takenAt}`);
  sceneImages.set(key, image);
  scene.images += 1;
  send(res, 201, image);
  return true;
}

createServer((req, res) => {
  const chunks = [];
  req.on("data", (c) => chunks.push(c));
  req.on("end", () => {
    const body = Buffer.concat(chunks);
    console.log(`${new Date().toLocaleTimeString()} ${req.method} ${req.url} (${body.length} bytes)`);

    if (req.headers.authorization !== `Bearer ${TOKEN}`) {
      res.setHeader("WWW-Authenticate", 'Bearer realm="SotA Lore API"');
      return send(res, 401, { error: "A valid, unexpired token is required" });
    }
    if (req.method === "GET" && req.url === "/api/v1/me") {
      return send(res, 200, {
        account: { id: 99, name: "Fake Companion" },
        token: { name: "fake", scopes: SCOPES, expires_at: null },
      });
    }
    if (scenesApi(req, res, body)) return;
    const endpoint = ENDPOINTS[req.url];
    if (req.method !== "POST" || !endpoint) {
      res.writeHead(404, { "Content-Type": "text/html" });
      return res.end("<h1>Not found</h1>");
    }
    if (!SCOPES.includes("upload")) return send(res, 403, { error: "This token doesn't have the upload scope" });
    if (failFirst > 0) {
      failFirst -= 1;
      return send(res, 503, { error: "Service unavailable (fake)" });
    }
    const file = fileField(req.headers["content-type"], body);
    if (!file) return send(res, 400, { error: "No file field" });
    if (file.content.length > 10 * 1024 * 1024) return send(res, 413, { error: "Over 10 MB" });

    const hash = createHash("sha256").update(file.content).digest("hex");
    if (seen.has(hash)) return send(res, 200, seen.get(hash));

    let json;
    try {
      json = JSON.parse(file.content.toString("utf8"));
    } catch {
      return send(res, 422, { error: "Couldn't import that: it isn't JSON" });
    }
    if (json.type !== endpoint.type) {
      const right = Object.entries(ENDPOINTS).find(([, e]) => e.type === json.type)?.[0];
      return send(res, 422, {
        error: `Couldn't import that: its type is ${json.type ?? "missing"}` + (right ? `; upload it to ${right}` : ""),
      });
    }
    const id = nextId++;
    const made = {
      id,
      filename: file.filename,
      api_version: json.apiVersion ?? null,
      created_at: new Date().toISOString(),
      ...endpoint.count(json),
      review_url: `http://localhost:${PORT}/adm/${REVIEW[json.type]}/${id}`,
    };
    seen.set(hash, made);
    send(res, 201, made);
  });
}).listen(PORT, "127.0.0.1", () => {
  console.log(`Fake SotA Lore API on http://localhost:${PORT}, token ${TOKEN}, scopes ${SCOPES.join(",")}`);
});
