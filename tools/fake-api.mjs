// A stand-in for the site's API, for trying the companion without a real token or touching the site.
// It answers GET /api/v1/me and the three upload endpoints the way sotalore/docs/api.md says, and
// keeps nothing on disk.
//
//   npm run fake-api                    # http://localhost:4567, token slt_fake (scope: upload)
//   FAIL_FIRST=2 npm run fake-api       # the first 2 uploads get a 503, to watch the retries
//   SCOPES=read npm run fake-api        # a token without upload
//
// In the companion, set the site to http://localhost:4567 and the token to slt_fake.

import { createServer } from "node:http";
import { createHash } from "node:crypto";

const PORT = Number(process.env.PORT ?? 4567);
const TOKEN = process.env.TOKEN ?? "slt_fake";
const SCOPES = (process.env.SCOPES ?? "upload").split(",");
let failFirst = Number(process.env.FAIL_FIRST ?? 0);

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

// Pulls the `file` field out of a multipart/form-data body.
function fileField(contentType, body) {
  const boundary = /boundary=(?:"([^"]+)"|([^;]+))/.exec(contentType ?? "");
  if (!boundary) return null;
  const marker = "--" + (boundary[1] ?? boundary[2]);
  const text = body.toString("latin1"); // byte for byte, so the hash matches the file's
  for (const part of text.split(marker)) {
    const split = part.indexOf("\r\n\r\n");
    if (split < 0) continue;
    const headers = part.slice(0, split);
    if (!/name="file"/.test(headers)) continue;
    const filename = /filename="([^"]*)"/.exec(headers)?.[1] ?? "upload.json";
    const content = Buffer.from(part.slice(split + 4).replace(/\r\n$/, ""), "latin1");
    return { filename, content };
  }
  return null;
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
