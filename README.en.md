# Cockpit Tools

This project is based on the original [CockpitTools](https://github.com/jlcodes99/cockpit-tools).

This repository documents only local additions and usage. It does not repeat the original project's full feature list. The repository contains build source only, not local databases, account data, or runtime configuration.

## Local Highlights

- Added `workbuddy2api`
- Added `qoder2api`

### `workbuddy2api`

Exposes the local WorkBuddy account pool as an OpenAI-compatible service. It supports Chat Completions and Responses endpoints, model listing, streaming, token refresh, account rotation, graded cooldown/breaker handling, and per-account concurrency limits. The API listens on port `7863`; the admin panel data is exposed on port `7864`.

### `qoder2api`

Exposes QoderWork login state as an OpenAI Chat Completions-compatible gateway. It refreshes local credentials, forwards requests to Qoder, maps display model names to upstream IDs, and aggregates upstream SSE when the client requests a non-streaming response. The gateway listens on port `7866`.

## Usage

```bash
npm install
npm run tauri:dev
```

Use the WorkBuddy and QoderWork pages to add accounts, then open the corresponding gateway panel to copy the local Base URL and API Key:

- WorkBuddy2API: `http://127.0.0.1:7863/v1`
- Qoder2API: `http://127.0.0.1:7866/v1`

Set these values as your OpenAI-compatible client's Base URL and API key. The service is intended for locally hosted or private-network clients only; do not expose it directly to the public internet.
