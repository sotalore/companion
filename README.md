# SotA Lore companion

A Windows desktop app that runs beside *Shroud of the Avatar* and connects the game to
[sotalore.com](https://www.sotalore.com). Built with [Tauri](https://tauri.app): a Rust core with a web UI.

Roadmap (tracked in sotalore/hq#3):

1. Upload the `sl-admin` plugin's export files to the site as they appear.
2. Check the plugin's all-skills export against the site and say what changed.
3. Upload screenshots to the gallery of the scene they were taken in.
4. Record and transcribe spoken notes while playing, labelled by scene, for an agent to turn into scene narratives.

How this repo works with the others: https://github.com/sotalore/hq.

## Phase 1: the export uploader

The app watches the game's `Lua/` folder. When `sl-admin` writes an export, the app reads its `type` and uploads
`recipes`, `items` and `finds` files to the site (`POST /api/v1/recipe_imports`, `/item_catalog_imports`,
`/finds_imports`, per `docs/api.md` in sotalore/sotalore). Other types (`gameSkills`, `skills`) are never sent.
Each upload's result and its review link show in the window. Closing the window leaves the app in the tray;
Quit is on the tray icon's right-click menu.

- **Token:** make an AI account at ADM → AI Access with a token that has the `upload` scope, and paste it in.
  It's kept in Windows Credential Manager (`sotalore-companion`, one per site URL), never in a file.
- **What gets sent:** only files written while the app runs (and auto upload is on), and only if those exact
  bytes weren't uploaded before. Files already in the folder are listed with an **Upload** button.
- **Retries:** network errors and 5xx are retried 5 times over about a minute; a 4xx is shown and not retried.
- Settings live in `%APPDATA%\com.sotalore.companion\settings.json`, and the upload history next to it in
  `uploads.json`.

### Build and run

Needs Rust (MSVC), the Visual Studio C++ build tools, Node and WebView2.

```sh
npm install
npm run dev                     # run it from source (debug build)
npx tauri build --no-bundle     # src-tauri/target/release/sotalore-companion.exe
cd src-tauri && cargo test      # unit tests; one uses the real Credential Manager
```

### Trying it without the site

`npm run fake-api` starts a stand-in API on `http://localhost:4567` (token `slt_fake`) that answers like the
real one: `201` for a new file, `200` for a repeat, `422` for a wrong type. `FAIL_FIRST=2` makes the first two
uploads fail with `503`, so you can watch the retries, and `SCOPES=read` gives a token without `upload`. In the
app, set the site to `http://localhost:4567` and paste `slt_fake`.
