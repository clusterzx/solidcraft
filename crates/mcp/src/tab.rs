//! The SolidCraft app in a browser tab, reached through `solidcraft-relay` (docs/web-mcp.md).
//!
//! The relay owns the transport (the tab's WebSocket, [`TabLink`]); this backend owns what a call
//! means. The tab runs control requests one at a time between frames and answers each when it is
//! done. A call that outlasts its wait is left running (the tab can't cancel it), and its reply is
//! kept for the `pending` tool, as with the desktop app (#1618). Optionally the relay keeps a
//! headless session of its own ([`HeadlessMode`]) for when no tab is connected.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use solidcraft_engine::control_pending;
use solidcraft_engine::io::vfs::{self, PathScope};

use crate::backend::{Backend, Headless, STILL_RUNNING};

/// How a tool error starts when no tab is connected (and no headless session stands in): the
/// request was not sent, so `batch` may be sent again as it is.
pub(crate) const NO_TAB: &str = "no SolidCraft tab is connected";

/// How often a long wait is reported on stderr.
const PROGRESS_EVERY: Duration = Duration::from_secs(30);

/// The relay's side of the tab: where requests go and where their replies come from.
pub trait TabLink: Send + Sync {
    /// Send a control request (`method`, `params`) to the tab. Its reply, `{ok, result | error}`,
    /// arrives on the receiver; a receiver whose sender is dropped means the tab went away before
    /// answering. `Err`: the request was not sent (no tab, or the request is too large).
    fn send(&self, method: &str, params: Value) -> Result<Receiver<Value>, String>;
    /// The connected tab's number (a new one for every tab that connects), or `None`.
    fn connection(&self) -> Option<u64>;
    /// `control.running`: the requests sent to the tab and not answered yet, from every session.
    fn running(&self) -> Value;
}

/// What the relay does without a tab (`SOLIDCRAFT_RELAY_HEADLESS`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeadlessMode {
    /// Calls need a tab; without one they say how to connect one (the default).
    Off,
    /// The tab while one is connected, the relay's own headless session while none is.
    Fallback,
    /// Always the relay's own headless session; tabs are not used.
    Only,
}

impl HeadlessMode {
    /// `off`, `fallback` or `only`.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "off" | "no" | "0" | "false" => Some(Self::Off),
            "fallback" => Some(Self::Fallback),
            "only" => Some(Self::Only),
            _ => None,
        }
    }
}

/// The relay's own headless session, shared by every MCP session, and the folder on the
/// server its files go to (its tool calls reach no other).
pub struct SharedHeadless {
    session: Mutex<Headless>,
    folder: PathBuf,
}

impl SharedHeadless {
    pub fn new(folder: PathBuf) -> Self {
        Self { session: Mutex::new(Headless::default()), folder }
    }
    pub fn folder(&self) -> &std::path::Path {
        &self.folder
    }
}

/// How long a call waits for the tab (tests shorten them).
#[derive(Clone, Copy, Debug)]
pub struct TabWaits {
    /// Most requests: as long as the desktop bridge waits in all (the app's 60 s and 240 s more).
    pub reply: Duration,
    /// `engine.batch`, which replaces many short requests.
    pub batch: Duration,
    /// How long a later call, or the `pending` tool, waits for a request still running.
    pub settle: Duration,
}

impl Default for TabWaits {
    fn default() -> Self {
        Self { reply: Duration::from_secs(300), batch: Duration::from_secs(3600), settle: Duration::from_secs(60) }
    }
}

/// Where a call goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
    /// The connected tab (its number).
    Tab(u64),
    Headless,
    /// Nowhere: no tab, and no headless session stands in.
    Nowhere,
}

/// A request sent to the tab that was still running when its call returned.
struct Outstanding {
    what: String,
    effect: control_pending::Effect,
    since: Instant,
    reply: Receiver<Value>,
}

/// What became of such a request once a later call waited it out (for `pending`).
enum Settled {
    Finished { what: String, reply: Value },
    Lost { what: String },
}

/// One MCP session's view of the tab (each session has its own requests still running).
pub struct TabBackend {
    link: Arc<dyn TabLink>,
    mode: HeadlessMode,
    headless: Option<Arc<SharedHeadless>>,
    /// The web app's address, for the "open it" hint.
    app_url: String,
    outstanding: Option<Outstanding>,
    settled: Option<Settled>,
    waits: TabWaits,
    /// Where the last call that went somewhere went.
    last: Option<Target>,
    notice: Option<String>,
}

/// A control reply as a call's result.
fn outcome(v: &Value) -> Result<Value, String> {
    if v.get("ok").and_then(Value::as_bool) == Some(true) {
        Ok(v.get("result").cloned().unwrap_or(Value::Null))
    } else {
        Err(v.get("error").and_then(Value::as_str).unwrap_or("error").to_string())
    }
}

impl TabBackend {
    /// `headless` is the relay's own session, used as `mode` says (`Off`: never).
    pub fn new(link: Arc<dyn TabLink>, mode: HeadlessMode, headless: Option<Arc<SharedHeadless>>, app_url: &str) -> Self {
        let mode = if headless.is_none() { HeadlessMode::Off } else { mode };
        Self {
            link,
            mode,
            headless,
            app_url: app_url.to_string(),
            outstanding: None,
            settled: None,
            waits: TabWaits::default(),
            last: None,
            notice: None,
        }
    }

    pub fn with_waits(mut self, waits: TabWaits) -> Self {
        self.waits = waits;
        self
    }

    fn target(&self) -> Target {
        let tab = self.link.connection();
        match (self.mode, tab) {
            (HeadlessMode::Only, _) => Target::Headless,
            (_, Some(n)) => Target::Tab(n),
            (HeadlessMode::Fallback, None) => Target::Headless,
            (HeadlessMode::Off, None) => Target::Nowhere,
        }
    }

    /// Tell the agent when a call goes somewhere else than the one before: another design.
    fn note(&mut self, now: Target) {
        let said = match (self.last, now) {
            (_, Target::Nowhere) => return,
            (None, Target::Headless) if self.mode == HeadlessMode::Fallback => Some(
                "No SolidCraft tab is connected: this call ran in the relay's own headless session, which holds a design of its own \
                 (not the one in a browser tab) and keeps its files on the server.",
            ),
            (Some(Target::Tab(_)), Target::Headless) => Some(
                "The SolidCraft tab went away: this call ran in the relay's own headless session, which holds another design than \
                 the tab did. Inspect it before changing it.",
            ),
            (Some(Target::Headless), Target::Tab(_)) => Some(
                "A SolidCraft tab is connected now: this call ran in the tab, whose design is not the relay's headless one. Inspect it \
                 before changing it.",
            ),
            (Some(Target::Tab(a)), Target::Tab(b)) if a != b => Some(
                "Another SolidCraft tab is connected now (a new or reloaded page): its design may not be the one the earlier calls \
                 changed. Inspect it before changing it.",
            ),
            _ => None,
        };
        if let Some(said) = said {
            self.notice = Some(said.to_string());
        }
        self.last = Some(now);
    }

    fn no_tab(&self) -> String {
        format!(
            "{NO_TAB}: open {}#mcp=<token> in a browser (the relay's token; the page keeps it and reconnects by itself) and keep \
             that tab in front, then send this call again. Nothing was sent",
            self.app_url
        )
    }

    fn headless_call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let Some(h) = self.headless.clone() else { return Err(self.no_tab()) };
        // A panic is caught where commands run (`Session::execute`); the session stays usable.
        let mut session = h.session.lock().unwrap_or_else(PoisonError::into_inner);
        // Its files: the relay's folder for them, and the open design's when that is a safe one.
        let scope = PathScope::new([h.folder.clone()]).with_design(session.session.path.as_deref());
        vfs::scoped(Some(scope), || session.call(method, params))
    }

    /// Wait up to `limit` for `reply`, saying on stderr how long it takes.
    fn wait(reply: &Receiver<Value>, limit: Duration, what: &str, since: Instant) -> Result<Value, RecvTimeoutError> {
        let start = Instant::now();
        loop {
            let left = limit.saturating_sub(start.elapsed());
            if left.is_zero() {
                return Err(RecvTimeoutError::Timeout);
            }
            match reply.recv_timeout(left.min(PROGRESS_EVERY)) {
                Err(RecvTimeoutError::Timeout) if start.elapsed() < limit => {
                    eprintln!("solidcraft-relay: {what} is still running in the tab ({} s); waiting for it", since.elapsed().as_secs());
                }
                r => return r,
            }
        }
    }

    /// The tool error for the request still running after its wait.
    fn still_running(&self, earlier: bool) -> String {
        let Some(o) = &self.outstanding else { return format!("{STILL_RUNNING} a request is still running in the tab") };
        let effect = o.effect.when_done();
        let secs = o.since.elapsed().as_secs();
        if earlier {
            format!(
                "{STILL_RUNNING} an earlier request, {}, is still running in the tab ({secs} s; {effect}), so this call was not sent. \
                 Call the `pending` tool to wait for its result, then send this call again",
                o.what
            )
        } else {
            format!(
                "{STILL_RUNNING} {} is still running in the tab after {secs} s ({effect}); don't send it again. Call the `pending` tool \
                 to wait for its result (up to {} s per call); other calls wait for it first",
                o.what,
                self.waits.settle.as_secs()
            )
        }
    }

    /// Before a new request: wait for one still running from an earlier call, keeping its end
    /// for `pending`. Still running after the wait: the new request is not sent.
    fn settle_earlier(&mut self) -> Result<(), String> {
        let Some(o) = self.outstanding.take() else { return Ok(()) };
        match o.reply.recv_timeout(self.waits.settle) {
            Ok(reply) => self.settled = Some(Settled::Finished { what: o.what, reply }),
            Err(RecvTimeoutError::Disconnected) => self.settled = Some(Settled::Lost { what: o.what }),
            Err(RecvTimeoutError::Timeout) => {
                self.outstanding = Some(o);
                return Err(self.still_running(true));
            }
        }
        Ok(())
    }

    /// `control.pending`: the end of the request still running from an earlier call.
    fn pending_status(&mut self) -> Result<Value, String> {
        if let Some(s) = self.settled.take() {
            return Ok(match s {
                Settled::Finished { what, reply } => json!({"pending": what, "done": true, "reply": reply}),
                Settled::Lost { what } => json!({"pending": what, "lost": true}),
            });
        }
        let Some(o) = self.outstanding.take() else { return Ok(json!({"pending": null})) };
        match o.reply.recv_timeout(self.waits.settle) {
            Ok(reply) => Ok(json!({"pending": o.what, "done": true, "reply": reply})),
            Err(RecvTimeoutError::Disconnected) => Ok(json!({"pending": o.what, "lost": true})),
            Err(RecvTimeoutError::Timeout) => {
                let r = json!({"pending": o.what, "done": false, "running_s": o.since.elapsed().as_secs()});
                self.outstanding = Some(o);
                Ok(r)
            }
        }
    }

    fn call_tab(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.settle_earlier()?;
        let what = control_pending::describe(method, &params);
        let effect = control_pending::effect(method, &params);
        let limit = if method == "engine.batch" { self.waits.batch } else { self.waits.reply };
        let reply = match self.link.send(method, params) {
            Ok(reply) => reply,
            Err(_) if self.link.connection().is_none() => return Err(self.no_tab()),
            Err(e) => return Err(e),
        };
        let since = Instant::now();
        match Self::wait(&reply, limit, &what, since) {
            Ok(v) => outcome(&v),
            Err(RecvTimeoutError::Disconnected) => Err(format!(
                "the SolidCraft tab went away during this call ({what}): it was closed, reloaded or replaced by another tab, after \
                 running the call or not. Inspect the design before sending a change again"
            )),
            Err(RecvTimeoutError::Timeout) => {
                eprintln!("solidcraft-relay: {what} is still running in the tab after {} s; left to the `pending` tool", since.elapsed().as_secs());
                self.outstanding = Some(Outstanding { what, effect, since, reply });
                Err(self.still_running(false))
            }
        }
    }
}

impl Backend for TabBackend {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let target = self.target();
        self.note(target);
        match (target, method) {
            (Target::Headless, _) => self.headless_call(method, params),
            (_, "control.pending") => self.pending_status(),
            (Target::Tab(_), "control.running") => Ok(self.link.running()),
            (Target::Nowhere, "control.running") => Ok(json!({"running": []})),
            (Target::Tab(_), _) => self.call_tab(method, params),
            (Target::Nowhere, _) => Err(self.no_tab()),
        }
    }
    fn has_ui(&self) -> bool {
        self.target() != Target::Headless
    }
    fn describe(&self) -> String {
        let tab = "connected to the SolidCraft app in a browser tab through solidcraft-relay (the user watches the viewport). File \
                   paths name the tab's browser storage (/opfs/…), not the server's disk";
        let folder = self.headless.as_ref().map(|h| h.folder.display().to_string()).unwrap_or_default();
        match self.mode {
            HeadlessMode::Only => format!("headless SolidCraft session in solidcraft-relay (no window; its files are in {folder} on the server)"),
            HeadlessMode::Fallback => format!(
                "{tab}. While no tab is connected, calls run in the relay's own headless session instead (another design; its files \
                 are in {folder} on the server), and their results say so"
            ),
            HeadlessMode::Off if self.link.connection().is_some() => tab.to_string(),
            HeadlessMode::Off => {
                format!("{tab}. No tab was connected when this session started; until one is (open {}#mcp=<token>), tool calls say so", self.app_url)
            }
        }
    }
    fn in_process(&self) -> bool {
        self.target() == Target::Headless
    }
    fn inline_images(&self) -> bool {
        true
    }
    fn take_notice(&mut self) -> Option<String> {
        self.notice.take()
    }
}
