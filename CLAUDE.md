# CLAUDE.md (companion)

@../hq/AGENTS.md

Notes for the `companion` agent. This repo is the SotA Lore companion: a Windows desktop app, built with Tauri
(a Rust core, a web UI), that runs on peregrin beside the game and connects it to the site. The roadmap and its
phases are the tracking issue sotalore/hq#3; each phase is an issue in this repo.

## What it reads and writes

The companion has no contract of its own yet: it reads other repos' contracts and follows them exactly.

- **The plugins' export files**, as `sotalore/sladmin`'s `EXPORT-FORMAT.md` describes them, from the game client's
  `Lua/` folder (`%APPDATA%\Catnip Games\Shroud of the Avatar\Lua`, and `...(DEV)\Lua` for the QA client; make the
  folder a setting). Tell files apart by their `type`, never by their name.
- **The site API**, as `sotalore/sotalore`'s `docs/api.md` describes it. Uploads need a token with the `upload`
  scope, from an AI account Darren makes at ADM → AI Access.
- **When a contract doesn't say what you need, open an issue in the repo that owns it** and message its agent
  (see "Messages" in `AGENTS.md`). Never guess at a format, and never work around the API.

## Rules for this app

- **The API token is a secret.** Keep it in the Windows Credential Manager (the `keyring` crate), never in a
  file in the repo, a log line or an issue.
- **Only send what Darren expects to be sent.** Upload only the file types the API takes, and say in the UI
  what was uploaded and where to review it (`review_url`).
- **Retries must be safe.** The API answers a repeated file with the first import (`200`), so retry network
  errors and 5xx with backoff; never retry a 4xx: the file or the request is the problem, so show the message.
- **The game comes first.** File watching and uploads must not slow the game: no busy loops, debounce file
  events (the plugins write files whole, but a file can be seen part-written), and keep CPU near zero when idle.
- Keep the Rust plain and commented for someone learning it: Darren knows Ruby well and Rust not yet, so a
  Ruby comparison in a comment helps where the idea is new.

## Environment

- The game, the plugins and this app run on **peregrin** (Windows). The checkout is
  `C:\dev\sotalore\companion`, beside `hq`, `sotalore`, `sladmin` and `slskills`.
- Toolchain: Rust (`rustup`, MSVC target, which needs the Visual Studio Build Tools' C++ workload), Node for the
  web UI, and WebView2 (already installed). Don't install system software yourself; ask Darren.

## Working style

- One topic per commit. Do not commit or push unless Darren asks; when he does, push straight to `main` (no
  branches or PRs). Say in the commit message what is verified on peregrin and what is not.
- Before starting work, find or open its issue in this repo (`bug` or `enhancement`), and put "Closes #N" in the
  commit message.
