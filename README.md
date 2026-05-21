# cockpit-codex-cli

Headless Codex Local API Gateway for AWS/Linux servers. It imports an existing Codex `auth.json`, refreshes OAuth tokens server-side, and exposes a small OpenAI-compatible HTTP API.

This project is extracted from the Codex Local API Service idea in `jlcodes99/cockpit-tools`, but is intentionally CLI-only: no Tauri, no GUI, no browser OAuth login.

## Supported Endpoints

- `GET /v1/models`
- `POST /v1/responses`
- `POST /v1/chat/completions`
- `POST /v1/images/generations`
- `POST /v1/images/edits`

Default upstream:

```text
https://chatgpt.com/backend-api/codex
```

Image model:

```text
gpt-image-2
```

## Build

```bash
cargo build --release
```

Binary:

```bash
target/release/cockpit-codex
```

## Publish to Your GitHub

For your GitHub account:

```bash
cd cockpit-codex-cli
git init
git add .
git commit -m "Initial Codex local API gateway CLI"
git branch -M main
git remote add origin https://github.com/isamu2025/cockpit-codex-cli.git
git push -u origin main
```

## Quick Start

Import a local Codex auth file:

```bash
cockpit-codex import-auth --file ~/.codex/auth.json --name main
```

Show the gateway key:

```bash
cockpit-codex key show
```

Start the gateway:

```bash
cockpit-codex serve --host 0.0.0.0 --port 8080
```

Call it:

```bash
curl http://SERVER_IP:8080/v1/responses \
  -H "Authorization: Bearer <gateway_api_key>" \
  -H "Content-Type: application/json" \
  -d '{"model":"gpt-5-codex","input":"hello"}'
```

Image generation:

```bash
curl http://SERVER_IP:8080/v1/images/generations \
  -H "Authorization: Bearer <gateway_api_key>" \
  -H "Content-Type: application/json" \
  -d '{"model":"gpt-image-2","prompt":"a clean product mockup","size":"1024x1024"}'
```

## Data Directory

Default:

```text
~/.cockpit-codex
```

Files:

- `config.toml`
- `gateway.key`
- `accounts/*.json`

Override:

```bash
COCKPIT_CODEX_HOME=/srv/cockpit-codex cockpit-codex status
```

or:

```bash
cockpit-codex --data-dir /srv/cockpit-codex status
```

## AWS Security Notes

The v1 server can bind to `0.0.0.0`, but it serves plain HTTP. That means the gateway Bearer key is visible to anyone who can observe traffic.

Recommended minimum AWS setup:

- Put the instance in a Security Group that only allows TCP `8080` from your own IP.
- Rotate the gateway key after setup:

```bash
cockpit-codex key rotate
```

Better production setup:

- Bind the gateway to `127.0.0.1:8080`.
- Put Caddy, Nginx, Cloudflare Tunnel, or an AWS Load Balancer with HTTPS in front of it.
- Add IP allowlists and request logging at the proxy layer.

## Commands

```bash
cockpit-codex import-auth --file ./auth.json --name main
cockpit-codex accounts list
cockpit-codex accounts remove <id-or-email>
cockpit-codex key show
cockpit-codex key rotate
cockpit-codex status
cockpit-codex serve --host 0.0.0.0 --port 8080
cockpit-codex test --base-url http://127.0.0.1:8080 --model gpt-5-codex
```

## License and API Stability

The source project uses `CC-BY-NC-SA-4.0`; confirm license compatibility before public or commercial distribution.

The upstream Codex endpoint is not a public stable API. Keep the gateway isolated behind this adapter so future header, path, or payload changes are easy to patch.
