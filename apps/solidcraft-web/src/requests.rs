//! Control requests made from inside the page, by the browser API (`api.rs`) and the agent bridge
//! (`bridge.rs`): the same control channel the desktop app's TCP server feeds. The window runs
//! them between frames; [`Requests::poll`] hands each reply to whoever asked.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::Duration;

use serde_json::{Value, json};
use solidcraft_ui_egui::ControlRequest;

/// Requests waiting for the window at once, from the API and the bridge together.
pub const MAX_PENDING: usize = 256;
/// Largest request, as JSON.
pub const MAX_JSON_BYTES: usize = 16 * 1024 * 1024;

/// What to do with a request's reply (`{ok, result | error}`).
type Done = Box<dyn FnOnce(Value)>;

struct Pending {
    reply: Receiver<Value>,
    done: Done,
}

#[derive(Clone)]
pub struct Requests {
    sender: Sender<ControlRequest>,
    pending: Rc<RefCell<Vec<Pending>>>,
    ctx: egui::Context,
}

impl Requests {
    pub fn new(sender: Sender<ControlRequest>, ctx: egui::Context) -> Self {
        Self { sender, pending: Rc::default(), ctx }
    }

    /// Queue `method` for the window; `done` gets its reply once it has run. `Err`: refused unrun.
    pub fn submit(&self, method: &str, params: Value, done: impl FnOnce(Value) + 'static) -> Result<(), String> {
        if self.pending.borrow().len() >= MAX_PENDING {
            return Err(format!("too many pending SolidCraft requests (maximum {MAX_PENDING})"));
        }
        let (request, reply) = ControlRequest::new(method, params);
        if self.sender.send(request).is_err() {
            return Err("SolidCraft is not running".into());
        }
        self.pending.borrow_mut().push(Pending { reply, done: Box::new(done) });
        self.ctx.request_repaint();
        Ok(())
    }

    /// Hand out the replies that came in (once per frame).
    pub fn poll(&self) {
        let mut completed = Vec::new();
        self.pending.borrow_mut().retain_mut(|pending| {
            let reply = match pending.reply.try_recv() {
                Ok(reply) => reply,
                Err(TryRecvError::Empty) => return true,
                Err(TryRecvError::Disconnected) => json!({"ok": false, "error": "SolidCraft request dropped"}),
            };
            let done = std::mem::replace(&mut pending.done, Box::new(|_| {}));
            completed.push((done, reply));
            false
        });
        // Release the queue before handing the replies out: a caller may submit another request.
        for (done, reply) in completed {
            done(reply);
        }
        if !self.pending.borrow().is_empty() {
            self.ctx.request_repaint_after(Duration::from_millis(16));
        }
    }
}
