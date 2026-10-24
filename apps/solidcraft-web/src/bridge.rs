//! The agent bridge: this tab connects out to `solidcraft-relay` (docs/web-mcp.md), which serves
//! MCP to agents and passes their tool calls here as control requests. A tab can't listen, so it
//! opens a WebSocket to `bridge` next to the app's own address (same origin; the app may live
//! under any path), shows the relay's token as its first message and then answers each request
//! `{id, method, params}` with `{id, ok, result | error}`, run by the same control handlers as the
//! desktop app's control channel and the browser API.
//!
//! Opt-in: opening the app as `…/#mcp=TOKEN` keeps the token in this browser (local storage) and
//! takes it out of the address; the app then connects whenever it starts, and reconnects with a
//! growing pause when the relay is not there. `#mcp=off` forgets it. `window.solidcraft.agent`
//! offers `connect(token?, url?)`, `disconnect()` and `status()`.

use std::cell::RefCell;
use std::rc::Rc;

use serde_json::{Value, json};
use solidcraft_engine::control_pending::NOT_RUN_HIDDEN;
use solidcraft_ui_egui::ui_kit::{self, Container, Surface};
use wasm_bindgen::{JsCast as _, JsValue, closure::Closure};

use crate::requests::{MAX_JSON_BYTES, Requests};

/// Where the token is kept in this browser.
const TOKEN_KEY: &str = "solidcraft.mcp.token";
/// The relay's close codes (apps/solidcraft-relay/src/bridge.rs): replaced by another tab, and
/// a token it refused. The tab does not reconnect by itself after either.
const CLOSE_REPLACED: u16 = 4000;
const CLOSE_UNAUTHORIZED: u16 = 4001;
/// The pause before reconnecting doubles from the first to the longest (ms).
const RETRY_FIRST_MS: f64 = 1000.0;
const RETRY_MOST_MS: f64 = 30_000.0;
/// The relay says it is there every 20 s; this long without a word, the connection is dead (ms).
const SILENCE_MS: f64 = 60_000.0;
/// How often the silence is checked (ms).
const CHECK_EVERY_MS: i32 = 15_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// No token: the bridge is off.
    Off,
    /// Connecting, or signed in and waiting for the relay's welcome.
    Connecting,
    Connected,
    /// The relay can't be reached (or the connection dropped): trying again.
    Retrying,
    /// Another tab took the relay's connection.
    Replaced,
    /// The relay refused the token.
    Refused,
}

impl State {
    fn name(self) -> &'static str {
        match self {
            State::Off => "off",
            State::Connecting => "connecting",
            State::Connected => "connected",
            State::Retrying => "retrying",
            State::Replaced => "replaced",
            State::Refused => "refused",
        }
    }
}

/// The JavaScript handlers of the open socket, kept alive as long as it is the bridge's.
struct Handlers {
    _open: Closure<dyn FnMut()>,
    _message: Closure<dyn FnMut(web_sys::MessageEvent)>,
    _close: Closure<dyn FnMut(web_sys::CloseEvent)>,
}

struct Inner {
    requests: Requests,
    ctx: egui::Context,
    token: Option<String>,
    /// A relay address given to `connect` (else `bridge` next to the app).
    url: Option<String>,
    socket: Option<web_sys::WebSocket>,
    handlers: Option<Handlers>,
    /// Which socket is the current one: events from an earlier one are ignored.
    generation: u64,
    state: State,
    /// The relay's number for this tab, once welcomed.
    tab: Option<u64>,
    /// Failed attempts in a row (the pause before the next).
    attempts: u32,
    /// When the relay was last heard from (ms, `performance.now()`).
    heard: f64,
    /// Why the last connection ended.
    reason: String,
}

#[derive(Clone)]
pub struct Bridge {
    inner: Rc<RefCell<Inner>>,
}

fn now() -> f64 {
    web_sys::window().and_then(|w| w.performance()).map_or(0.0, |p| p.now())
}

fn storage() -> Option<web_sys::Storage> {
    web_sys::window().and_then(|w| w.local_storage().ok().flatten())
}

/// The token kept in this browser, if any.
fn stored_token() -> Option<String> {
    storage().and_then(|s| s.get_item(TOKEN_KEY).ok().flatten()).filter(|t| !t.is_empty())
}

fn store_token(token: Option<&str>) {
    if let Some(s) = storage() {
        let _ = match token {
            Some(t) => s.set_item(TOKEN_KEY, t),
            None => s.remove_item(TOKEN_KEY),
        };
    }
}

/// Take `#mcp=TOKEN` out of the address (it is kept in local storage instead) and say it.
fn take_fragment_token() -> Option<String> {
    let window = web_sys::window()?;
    let location = window.location();
    let (token, rest) = crate::mcp_token_from_fragment(&location.hash().ok()?)?;
    let token = js_sys::decode_uri_component(&token).ok().and_then(|t| t.as_string()).unwrap_or(token);
    let path = format!("{}{}", location.pathname().unwrap_or_default(), location.search().unwrap_or_default());
    let address = if rest.is_empty() { path } else { format!("{path}#{rest}") };
    if let Ok(history) = window.history() {
        let _ = history.replace_state_with_url(&JsValue::NULL, "", Some(&address));
    }
    Some(token)
}

/// `bridge` next to the app's own address, as ws:// or wss://.
fn default_url() -> Option<String> {
    let href = web_sys::window()?.location().href().ok()?;
    let url = web_sys::Url::new_with_base("bridge", &href).ok()?;
    url.set_hash("");
    url.set_search("");
    url.set_protocol(if url.protocol() == "https:" { "wss:" } else { "ws:" });
    Some(url.href())
}

/// The tab is in the background, where the browser stops drawing frames (and so running
/// requests) until it is in front again.
fn hidden() -> bool {
    web_sys::window().and_then(|w| w.document()).is_some_and(|d| d.visibility_state() == web_sys::VisibilityState::Hidden)
}

impl Bridge {
    /// Set up the bridge (`window.solidcraft.agent`), and connect when this browser has a token.
    pub fn install(requests: Requests, ctx: egui::Context) -> Self {
        let inner = Inner {
            requests,
            ctx,
            token: None,
            url: None,
            socket: None,
            handlers: None,
            generation: 0,
            state: State::Off,
            tab: None,
            attempts: 0,
            heard: 0.0,
            reason: String::new(),
        };
        let bridge = Bridge { inner: Rc::new(RefCell::new(inner)) };
        if let Err(e) = bridge.install_api() {
            log::warn!("SolidCraft agent bridge API: {e:?}");
        }
        bridge.watch_silence();
        match take_fragment_token() {
            Some(t) if t == "off" || t.is_empty() => {
                store_token(None);
            }
            Some(t) => {
                store_token(Some(&t));
                bridge.connect(Some(t), None);
            }
            None => {
                if let Some(t) = stored_token() {
                    bridge.connect(Some(t), None);
                }
            }
        }
        bridge
    }

    pub fn state(&self) -> State {
        self.inner.borrow().state
    }

    /// Connect with `token` (`None`: the one kept), to `url` (`None`: `bridge` next to the app).
    pub fn connect(&self, token: Option<String>, url: Option<String>) {
        {
            let mut inner = self.inner.borrow_mut();
            if let Some(t) = token {
                inner.token = Some(t);
            } else if inner.token.is_none() {
                inner.token = stored_token();
            }
            if url.is_some() {
                inner.url = url;
            }
            inner.attempts = 0;
        }
        self.open();
    }

    /// End the connection and forget the token: the app no longer connects when it starts.
    pub fn disconnect(&self) {
        let mut inner = self.inner.borrow_mut();
        inner.generation += 1;
        if let Some(s) = inner.socket.take() {
            let _ = s.close_with_code_and_reason(1000, "disconnected in the app");
        }
        inner.handlers = None;
        inner.token = None;
        inner.state = State::Off;
        inner.tab = None;
        store_token(None);
        inner.ctx.request_repaint();
    }

    fn status(&self) -> Value {
        let inner = self.inner.borrow();
        json!({
            "state": inner.state.name(),
            "connected": inner.state == State::Connected,
            "tab": inner.tab,
            "url": inner.url.clone().or_else(default_url),
            "reason": (!inner.reason.is_empty()).then_some(&inner.reason),
        })
    }

    /// A new socket (the one before, if any, is dropped).
    fn open(&self) {
        let mut inner = self.inner.borrow_mut();
        inner.generation += 1;
        let generation = inner.generation;
        if let Some(s) = inner.socket.take() {
            let _ = s.close_with_code_and_reason(1000, "reconnecting");
        }
        inner.handlers = None;
        let Some(token) = inner.token.clone() else {
            inner.state = State::Off;
            return;
        };
        let Some(url) = inner.url.clone().or_else(default_url) else {
            inner.state = State::Retrying;
            inner.reason = "no address for the relay".into();
            return;
        };
        let socket = match web_sys::WebSocket::new(&url) {
            Ok(s) => s,
            Err(e) => {
                inner.state = State::Retrying;
                inner.reason = format!("{e:?}");
                drop(inner);
                self.retry_later(generation);
                return;
            }
        };
        let this = self.clone();
        let opened = socket.clone();
        let open = Closure::<dyn FnMut()>::new(move || {
            if this.inner.borrow().generation != generation {
                return;
            }
            let hello = json!({"type": "hello", "token": token, "client": "solidcraft-web", "version": env!("CARGO_PKG_VERSION")});
            let _ = opened.send_with_str(&hello.to_string());
        });
        let this = self.clone();
        let message = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |e: web_sys::MessageEvent| this.on_message(generation, &e));
        let this = self.clone();
        let close = Closure::<dyn FnMut(web_sys::CloseEvent)>::new(move |e: web_sys::CloseEvent| this.on_close(generation, &e));
        socket.set_onopen(Some(open.as_ref().unchecked_ref()));
        socket.set_onmessage(Some(message.as_ref().unchecked_ref()));
        socket.set_onclose(Some(close.as_ref().unchecked_ref()));
        inner.handlers = Some(Handlers { _open: open, _message: message, _close: close });
        inner.socket = Some(socket);
        inner.state = State::Connecting;
        inner.heard = now();
        inner.ctx.request_repaint();
    }

    fn send(socket: &web_sys::WebSocket, reply: &Value) {
        if socket.ready_state() == web_sys::WebSocket::OPEN {
            let _ = socket.send_with_str(&reply.to_string());
        }
    }

    fn on_message(&self, generation: u64, e: &web_sys::MessageEvent) {
        let (socket, requests, ctx) = {
            let mut inner = self.inner.borrow_mut();
            if inner.generation != generation {
                return;
            }
            inner.heard = now();
            let Some(socket) = inner.socket.clone() else { return };
            (socket, inner.requests.clone(), inner.ctx.clone())
        };
        let Some(text) = e.data().as_string() else { return };
        if text.len() > MAX_JSON_BYTES {
            let _ = socket.close_with_code_and_reason(1009, "request too large");
            return;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&text) else { return };
        match msg.get("type").and_then(Value::as_str) {
            Some("ping") => Self::send(&socket, &json!({"type": "pong"})),
            Some("welcome") => {
                let mut inner = self.inner.borrow_mut();
                inner.state = State::Connected;
                inner.tab = msg.get("tab").and_then(Value::as_u64);
                inner.attempts = 0;
                inner.reason.clear();
                ctx.request_repaint();
            }
            Some(_) => {}
            None => {
                let (Some(id), Some(method)) = (msg.get("id").cloned(), msg.get("method").and_then(Value::as_str)) else { return };
                let params = msg.get("params").cloned().filter(|p| !p.is_null()).unwrap_or(json!({}));
                // A hidden tab draws no frames, so the request would wait unseen: say so now.
                if hidden() {
                    let error = format!(
                        "{NOT_RUN_HIDDEN}, where the browser pauses it, so `{}` was not run: bring the tab to the front (or open \
                         SolidCraft in a window of its own) and send it again",
                        method.chars().take(64).collect::<String>()
                    );
                    Self::send(&socket, &json!({"id": id, "ok": false, "error": error}));
                    return;
                }
                let replier = socket.clone();
                let answer_id = id.clone();
                let submitted = requests.submit(method, params, move |mut reply| {
                    reply["id"] = answer_id;
                    Self::send(&replier, &reply);
                });
                if let Err(error) = submitted {
                    Self::send(&socket, &json!({"id": id, "ok": false, "error": error}));
                }
                ctx.request_repaint();
            }
        }
    }

    fn on_close(&self, generation: u64, e: &web_sys::CloseEvent) {
        let retry = {
            let mut inner = self.inner.borrow_mut();
            if inner.generation != generation {
                return;
            }
            inner.socket = None;
            inner.tab = None;
            inner.reason = e.reason();
            inner.state = match e.code() {
                CLOSE_REPLACED => State::Replaced,
                CLOSE_UNAUTHORIZED => State::Refused,
                _ => State::Retrying,
            };
            inner.ctx.request_repaint();
            inner.state == State::Retrying
        };
        if retry {
            self.retry_later(generation);
        }
    }

    /// Try again after a pause that grows with every failed attempt.
    fn retry_later(&self, generation: u64) {
        let wait = {
            let mut inner = self.inner.borrow_mut();
            inner.attempts = inner.attempts.saturating_add(1);
            (RETRY_FIRST_MS * 2f64.powi(inner.attempts.min(8) as i32 - 1)).min(RETRY_MOST_MS)
        };
        let this = self.clone();
        let again = Closure::once_into_js(move || {
            if this.inner.borrow().generation == generation && this.state() == State::Retrying {
                this.open();
            }
        });
        if let Some(w) = web_sys::window() {
            let _ = w.set_timeout_with_callback_and_timeout_and_arguments_0(again.unchecked_ref(), wait as i32);
        }
    }

    /// A connection the relay stopped speaking on is closed, and so tried again.
    fn watch_silence(&self) {
        let this = self.clone();
        let check = Closure::<dyn FnMut()>::new(move || {
            let inner = this.inner.borrow();
            if matches!(inner.state, State::Connecting | State::Connected)
                && now() - inner.heard > SILENCE_MS
                && let Some(s) = &inner.socket
            {
                // Its close event reconnects.
                let _ = s.close_with_code_and_reason(4002, "the relay went quiet");
            }
        });
        if let Some(w) = web_sys::window() {
            let _ = w.set_interval_with_callback_and_timeout_and_arguments_0(check.as_ref().unchecked_ref(), CHECK_EVERY_MS);
        }
        // The check runs as long as the page.
        check.forget();
    }

    /// `window.solidcraft.agent`: `connect(token?, url?)`, `disconnect()`, `status()`.
    fn install_api(&self) -> Result<(), JsValue> {
        let window = web_sys::window().ok_or_else(|| crate::api::error("no window"))?;
        let solidcraft = js_sys::Reflect::get(&window, &"solidcraft".into())?;
        if !solidcraft.is_object() {
            return Err(crate::api::error("window.solidcraft is missing"));
        }
        let object = js_sys::Object::new();
        let (a, b, c) = (self.clone(), self.clone(), self.clone());
        let to_js = |v: Value| js_sys::JSON::parse(&v.to_string()).unwrap_or(JsValue::NULL);
        let connect = Closure::<dyn Fn(JsValue, JsValue) -> JsValue>::new(move |token: JsValue, url: JsValue| {
            let token = token.as_string().filter(|t| !t.is_empty());
            if let Some(t) = &token {
                store_token(Some(t));
            }
            a.connect(token, url.as_string().filter(|u| !u.is_empty()));
            to_js(a.status())
        });
        let disconnect = Closure::<dyn Fn() -> JsValue>::new(move || {
            b.disconnect();
            to_js(b.status())
        });
        let status = Closure::<dyn Fn() -> JsValue>::new(move || to_js(c.status()));
        js_sys::Reflect::set(&object, &"connect".into(), &connect.into_js_value())?;
        js_sys::Reflect::set(&object, &"disconnect".into(), &disconnect.into_js_value())?;
        js_sys::Reflect::set(&object, &"status".into(), &status.into_js_value())?;
        js_sys::Reflect::set(&solidcraft, &"agent".into(), &object)?;
        Ok(())
    }

    /// The bridge's state at the foot of the viewport while it is on: whether an agent can reach
    /// this tab, and a way to turn that off (or take the connection back).
    pub fn show(&self, app: &solidcraft_ui_egui::SolveApp, ctx: &egui::Context) {
        let state = self.state();
        let (text, dot) = match state {
            State::Off => return,
            State::Connected => ("web.agent.connected", true),
            State::Connecting => ("web.agent.connecting", false),
            State::Retrying => ("web.agent.retrying", false),
            State::Replaced => ("web.agent.replaced", false),
            State::Refused => ("web.agent.refused", false),
        };
        let t = solidcraft_ui_egui::theme::Tokens::get();
        let area = app.viewport.rect.unwrap_or_else(|| ctx.content_rect());
        let at = egui::pos2(area.right() - 12.0, area.bottom() - 12.0);
        let mut act = None;
        Container::new(ctx, Surface::Toast)
            .area(egui::Id::new("web_agent_bridge"))
            .order(egui::Order::Foreground)
            .pivot(egui::Align2::RIGHT_BOTTOM)
            .fixed_pos(at)
            .show(ctx, |ui| {
                Container::popup_frame(ui.style()).fill(t.panel).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let (rect, _) = ui.allocate_exact_size(egui::vec2(t.space_10, t.space_10), egui::Sense::hover());
                        ui.painter().circle_filled(rect.center(), t.space_10 / 2.0, if dot { t.accent } else { t.text_dim });
                        ui.label(egui::RichText::new(app.tr(text)).color(if dot { t.text } else { t.text_dim }))
                            .on_hover_text(app.tr("web.agent.hint"));
                        let (label, what) = if state == State::Replaced { ("web.agent.reconnect", true) } else { ("web.agent.disconnect", false) };
                        if ui_kit::button(ui, app.tr(label), ui_kit::Button::Small).clicked() {
                            act = Some(what);
                        }
                    });
                });
            });
        match act {
            Some(true) => self.connect(None, None),
            Some(false) => self.disconnect(),
            None => {}
        }
    }
}
