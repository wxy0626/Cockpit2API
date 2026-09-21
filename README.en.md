# Cockpit2API

This project is based on the original [CockpitTools](https://github.com/jlcodes99/cockpit-tools).

This repository documents only local additions and usage. It does not repeat the original project's full feature list. The repository contains build source only, not local databases, account data, or runtime configuration.

## Local Highlights

- Added `workbuddy2api`
- Added `qoder2api`

### `workbuddy2api`

Exposes the local WorkBuddy account pool as an OpenAI-compatible service. It supports Chat Completions and Responses endpoints, model listing, streaming, token refresh, account rotation, graded cooldown/breaker handling, and per-account concurrency limits. The API listens on port `7863`; the admin panel data is exposed on port `7864`.

### `qoder2api`

Exposes QoderWork login state as an OpenAI Chat Completions-compatible gateway. It refreshes local credentials, forwards requests to Qoder, maps display model names to upstream IDs, and aggregates upstream SSE when the client requests a non-streaming response. The gateway listens on port `7866`.

### `cindy2api`

Adds the Cindy platform gateway and account management. The gateway can discover local Cindy credentials or add accounts by phone number, email verification code, or OAuth.

- Provides OpenAI-compatible `/v1/models` and `/v1/chat/completions`
- Supports streaming, non-streaming, account pooling, and health checks
- Supports Cindy CN phone login, international email login, OAuth, and local import
- Opens a dedicated verification window when required
- Keeps gateway keys isolated from upstream credentials
- Listens on port `7865`

### WorkBuddy automation and hardening

- Auto check-in, travel, and growth-center tasks can be configured separately
- Backend scheduling includes run logs and manual triggers
- The native gateway adds single-instance locking, port takeover, token refresh, account rotation, and inline per-account concurrency limits that also feed account selection weights

### Protocol and OAuth compatibility

- Completes Responses-to-Chat protocol translation for the WorkBuddy gateway and enables the Codex `spawn_agent` flow
- Uses the trusted local Chrome profile for OAuth authorization flows

### Incremental build speed

Moves frontend assets, the application binary, and the Tauri context into a dedicated `context` crate so frontend output changes no longer invalidate the main business library. Build-script fingerprints are deterministic, Windows links with `rust-lld`, TypeScript type checking is incremental, and `npm run build:app` is the unified release-build entry point.

## Usage

```bash
npm install
npm run tauri:dev
```

Use the WorkBuddy and QoderWork pages to add accounts, then open the corresponding gateway panel to copy the local Base URL and API Key:

- WorkBuddy2API: `http://127.0.0.1:7863/v1`
- Qoder2API: `http://127.0.0.1:7866/v1`

Set these values as your OpenAI-compatible client's Base URL and API key. The service is intended for locally hosted or private-network clients only; do not expose it directly to the public internet.
