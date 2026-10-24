# MCP for the web app (solidcraft-relay)

An AI agent such as Claude Code can drive the design open in a SolidCraft **browser tab**, with
the same tools as the desktop MCP server ([mcp.md](mcp.md)): `execute`, `batch`,
`inspect_design`, `screenshot` (the picture reaches the model), `pending`, `open`, `save` and the
rest. The agent connects over HTTPS to `https://HOST/mcp`; the person watches the agent model in
the tab.

## How it fits together

```text
MCP client ──HTTPS POST /mcp (Authorization: Bearer TOKEN)──► reverse proxy (TLS) ──► nginx (web image)
                                                                                     ├─ /         the web app (static)
                                                                                     ├─ /mcp    ─► solidcraft-relay
                                                                                     └─ /bridge ─► solidcraft-relay (WebSocket)
browser tab (the web app) ──wss://HOST/bridge (TOKEN as its first message)─────────────────┘
        └─ bridge.rs ─► the app's control requests (as the desktop control channel)
```

- A browser tab can't listen for connections, so it **connects out**: the web app opens a
  WebSocket to `bridge` next to its own address (same origin, any path) and waits for requests.
- The MCP server runs in **solidcraft-relay** (`apps/solidcraft-relay`), not in the tab. Its
  tools are the desktop server's (`crates/mcp`); only the backend differs (`TabBackend`), which
  sends each control request (`engine.execute`, `document.inspect`, `ui.screenshot`…) over the
  bridge and waits for the tab's answer. A tool call is synchronous and may wait minutes for a
  long kernel operation, which the tab's single UI thread could not do itself.
- The tab answers each request through the same control handlers as the desktop app's control
  channel and the [browser API](browser-api.md) (`window.solidcraft`), between frames.

## Set it up (Docker)

`packaging/docker/compose.yaml` runs two services: `web` (nginx with the app, as before) and
`relay`. nginx passes `/mcp` and `/bridge` on to the relay, which has no port of its own.

```sh
cd packaging/docker
cp .env.example .env
openssl rand -hex 32            # put it in .env as SOLIDCRAFT_RELAY_TOKEN=…; chmod 600 .env
docker compose up -d            # pulls ghcr.io/<owner>/solidcraft-web and solidcraft-relay
```

`.env` also picks whose images to pull and which tag (`SOLIDCRAFT_IMAGE_OWNER`,
`SOLIDCRAFT_TAG`; a fork publishes its own, see below) and the host port (`SOLIDCRAFT_PORT`,
default 8080). `docker compose up -d --build` builds both images from the checkout instead. Both
containers run read-only, without capabilities and with `no-new-privileges`; the relay's image
has no shell (distroless) and runs as an unprivileged user.

Put a reverse proxy with a certificate in front of port 8080, as for the app alone. It must pass
WebSocket upgrades on (`/bridge`) and allow long requests (`/mcp`: a tool call can take as long
as the tab works on it, a batch up to an hour). With Caddy that is all there is to it:

```caddyfile
cad.example.de {
    reverse_proxy 127.0.0.1:8080
}
```

Caddy and Traefik pass WebSockets on by themselves and don't cut long responses short. With
nginx (or Nginx Proxy Manager), turn on "Websockets Support" and set `proxy_read_timeout 3800s;`. The proxy should pass the original
`Host` (or `X-Forwarded-Host`) and `X-Forwarded-Proto`: the relay compares the page's origin with
it, and names the app's address in its hints (`SOLIDCRAFT_RELAY_PUBLIC_URL` sets that address
outright).

Without the relay (`docker compose up -d web`, or the web image alone) the app works as before:
`/mcp` and `/bridge` answer 502 and the tab simply doesn't connect.

### Images from a fork

The `Docker` workflow builds both images. In tangency-labs/solidcraft it publishes them on
pushes to `main` (`edge`) and on releases; in a fork it builds and smoke-tests them, and
publishes only when run by hand (Actions › Docker › Run workflow, on a branch: tagged with the
branch name, such as `ghcr.io/clusterzx/solidcraft-relay:web-mcp`). New packages on GitHub's
registry are private: make them public in the package's settings, or `docker login ghcr.io`
with a token that can read packages on the machine that pulls them.

## Connect a tab

Open the app once with the token in the address's fragment:

```text
https://cad.example.de/#mcp=<TOKEN>
```

The fragment never reaches a server. The app keeps the token in this browser (local storage),
takes it out of the address at once, connects, and from then on connects whenever it starts. A
chip at the foot of the viewport says **AI agent connected** (or connecting, not reachable,
refused, replaced) and has a **Disconnect** button, which ends the connection and forgets the
token. `#mcp=off` forgets it too.

From the browser's console (or a page script):

```js
solidcraft.agent.status();                 // {state: "connected", connected: true, tab: 3, url: "wss://…/bridge", reason: null}
solidcraft.agent.connect("<TOKEN>");       // keep a token and connect; connect() reconnects with the kept one
solidcraft.agent.connect("<TOKEN>", "wss://other.example/bridge");  // to a relay elsewhere (its SOLIDCRAFT_RELAY_ALLOWED_ORIGIN must allow this page)
solidcraft.agent.disconnect();             // end it and forget the token
```

**Keep the tab in front.** A browser draws no frames for a tab in the background or a minimised
window, and the app runs requests between frames. A request that arrives while the tab is hidden
is refused unrun at once (`not run: the SolidCraft tab is in the background…`), so the agent
can send it again later. Give SolidCraft a window of its own, or install it as an app (File › Install app…), and leave that window visible while the agent works.

**One tab at a time.** The newest tab to connect wins: the one before is closed with a reason,
says "Another tab took over the AI agent connection" and offers **Reconnect**; it doesn't come
back by itself (two tabs would otherwise take turns forever). Requests still running in the old
tab are reported lost to the agent. When the tab changes (a new tab, or the same one reloaded),
the agent's next result says so: its design may not be the one the earlier calls changed.

## Connect an agent

Claude Code:

```sh
claude mcp add --transport http solidcraft https://cad.example.de/mcp --header "Authorization: Bearer <TOKEN>"
```

Any client that speaks MCP over Streamable HTTP works the same way, for example a project's
`.mcp.json`:

```json
{
  "mcpServers": {
    "solidcraft": {
      "type": "http",
      "url": "https://cad.example.de/mcp",
      "headers": { "Authorization": "Bearer ${SOLIDCRAFT_RELAY_TOKEN}" }
    }
  }
}
```

The server's instructions end with `Backend: connected to the SolidCraft app in a browser tab
through solidcraft-relay …`, so the bundled skills treat it as a bridged session (the user
watches). Without a tab, every tool call answers `no SolidCraft tab is connected: open
https://HOST/#mcp=<token> in a browser … Nothing was sent`.

## Tools and their behaviour

The tools are the desktop server's, with the bridged backend's rules:

- **Timeouts.** A call waits up to 5 minutes for the tab (a batch up to an hour). A request that
  takes longer is not cancelled: the call answers `still running: …` and the `pending` tool
  waits for its result (up to 60 s per call); other calls in the same session wait for it first.
  `running` lists what the tab is running for every session. Kernel operations run on the
  tab's main thread, which freezes the page while they compute; that is expected.
- **Screenshots.** The tab sends the picture back in its reply (`ui.screenshot`/`ui.render` with
  `inline: true`, see [control-protocol.md](control-protocol.md)): the window as the browser
  shows it (the default), or a render of the model (`source: "model"`, drawn on the CPU in the
  tab). A `path` keeps a copy in the browser's storage as well. The window picture is eframe's
  capture of the next presented frame (`ViewportCommand::Screenshot`), which the web runner
  supports with WebGPU and WebGL2 alike; it needs the tab in front, like everything else.
- **Files.** Paths name the browser's own virtual file system, not the server's disk: the designs
  the app keeps in the browser are under `/opfs/` (`save {path: "/opfs/part.solidcraft"}`, then
  `open` it; they show in File › Files in this browser…). The relay checks no paths against
  folders of its own and sends no `allow_paths`: what a tab can reach is what the page can
  reach. Export or download to the computer stays with the person at the tab.
- **Sessions.** Each MCP session (`Mcp-Session-Id`) has its own pending request, as each
  desktop bridge has its own connection; all sessions share the one tab and its design.

## A headless session on the server (optional)

`SOLIDCRAFT_RELAY_HEADLESS` gives the relay a design of its own, as `solidcraft-cli mcp` without an
app:

| Value | While a tab is connected | While none is |
|---|---|---|
| `off` (default) | the tab | an error: connect a tab |
| `fallback` | the tab | the relay's headless session |
| `only` | the relay's headless session | the relay's headless session |

The headless session is shared by every MCP session and lives as long as the relay. Its files
stay on the server, in `SOLIDCRAFT_RELAY_DATA` (`/data` in the image, the `relay-data` volume in
compose.yaml); its tool calls reach no other folder. Its pictures are drawn on the CPU. In
`fallback`, a call that goes to the other side than the one before carries a note saying so:
the tab and the headless session hold different designs.

## Settings

| Variable | Default | Meaning |
|---|---|---|
| `SOLIDCRAFT_RELAY_TOKEN` | (required) | The token. At least 16 printable characters; `openssl rand -hex 32` makes a good one |
| `SOLIDCRAFT_RELAY_TOKEN_FILE` | | Read the token from this file instead (a Docker secret) |
| `SOLIDCRAFT_RELAY_LISTEN` | `127.0.0.1:8090` (`0.0.0.0:8090` in the image) | Address to listen on |
| `SOLIDCRAFT_RELAY_ALLOWED_ORIGIN` | | Other sites (`https://cad.example`, comma-separated) whose pages may connect a tab or call `/mcp` from a browser |
| `SOLIDCRAFT_RELAY_PUBLIC_URL` | from each request | The app's address, for hints |
| `SOLIDCRAFT_RELAY_HEADLESS` | `off` | `off`, `fallback` or `only` (above) |
| `SOLIDCRAFT_RELAY_DATA` | `./solidcraft-relay-data` (`/data` in the image) | The headless session's folder |

`solidcraft-relay --health` asks a running relay whether it answers (the image's health check);
`GET /healthz` answers `ok` without a token.

## Security

**Whoever has the token controls the tab**: they can read every design the app keeps in that
browser, change and save designs there, export, and look at the window. Treat it like a
password.

- Prefer a LAN or a VPN (WireGuard, Tailscale) to exposing the relay on the internet; if you do
  expose it, use a long random token and HTTPS only.
- Rotate the token by changing it in `.env` and `docker compose up -d`: agents and tabs with the
  old one are refused (tabs show "refused"; open the app once more with `#mcp=<new token>`).
- Every `/mcp` request needs `Authorization: Bearer <token>` (401 otherwise); the tab shows the
  token as its first WebSocket message. Tokens are compared in constant time and never logged.
- A browser page may open the bridge (or call `/mcp`) only from the relay's own site, or one
  named in `SOLIDCRAFT_RELAY_ALLOWED_ORIGIN` (403 otherwise), against DNS rebinding and other
  sites' pages. Clients that are not browsers send no origin and need the token alone.
- The token stays in the browser's local storage for that site: anyone who can use that browser
  profile (or run script on that site) can read it. Disconnect forgets it.
- Everything the relay receives is treated as hostile: request heads (16 KiB) and bodies
  (16 MiB) have caps, as do the tab's replies (64 MiB), sessions (64), connections (64),
  bridge connections (8) and requests waiting in a tab (256). A tab that stays silent for 60 s
  is dropped, and one that doesn't show the token within 10 s too.

## Protocol details

`/mcp` speaks MCP's Streamable HTTP transport (revisions 2025-06-18, 2025-03-26 and 2024-11-05),
with JSON responses only: a POST with a request gets `200` and its response; one with only
notifications or responses gets `202`; `initialize` starts a session and returns its
`Mcp-Session-Id`, which later requests carry (missing: `400`; unknown or ended: `404`, start
again with `initialize`); an unsupported `MCP-Protocol-Version` gets `400`; GET gets `405` (no
server-sent event stream); DELETE ends the session. Sessions live in the relay's memory: after
a restart, clients start new ones.

`/bridge` is a WebSocket of JSON text messages. The tab's first message is
`{"type": "hello", "token": …}`; the relay answers `{"type": "welcome", "tab": n}` or closes with
code 4001. Then the relay sends control requests `{"id", "method", "params"}` and the tab
answers each with `{"id", "ok", "result" | "error"}`, as the desktop control channel's lines
([control-protocol.md](control-protocol.md)). The relay sends `{"type": "ping"}` every 20 s and
the tab answers `{"type": "pong"}`; either side closes a connection it hasn't heard from for
60 s. Close code 4000 means another tab took over. After any other close the tab reconnects,
waiting 1 s, 2 s, 4 s … up to 30 s between attempts.

## Tests

- `cargo test -p solidcraft-relay`: the relay on a real socket with a pretend tab: tokens,
  initialize, tools/list and tools/call, no tab, a second tab taking over, timeouts and
  `pending`, a tab going away or silent, oversized messages, origins, the headless fallback.
- `cargo test -p solidcraft-mcp`: `TabBackend` (timeouts, pending, notes, hidden tabs, headless
  modes) and inline screenshots; the stdio server's tests run unchanged.
- `cargo xtask web-mcp-test --site dist/web`: the real thing: the relay, the web build (behind a
  server that passes `/bridge` on, as nginx does) in headless Chrome opened with `#mcp=TOKEN`,
  and an MCP client: `execute solid.box`, `inspect_design`, `screenshot` (kept in `--out` to look
  at), `batch`, `pending`, `save` and `open`, no tab after `disconnect()`, a second tab taking
  over. Build the web app first (`trunk build` in `apps/solidcraft-web`); `CHROME` names the
  browser if it isn't found.
- `packaging/docker/relay-smoke-test.sh`: the relay image alone and behind the web image's nginx.

## Limits

- One tab at a time; no choice between several.
- The tab must stay visible (see above). A long kernel operation freezes the page until it is
  done.
- No server-sent events: progress and notifications from the server are not streamed.
- No OAuth: the token is a static bearer token.
- Through MCP an agent uses the tools; raw control methods for the UI (`ui.click`, `ui.drag`…)
  are not tools.
