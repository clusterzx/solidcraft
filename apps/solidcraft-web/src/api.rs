//! Promise-based access to the running app's existing control channel.

use serde_json::{Value, json};
use wasm_bindgen::{JsValue, closure::Closure};

use crate::requests::{MAX_JSON_BYTES, Requests};

#[derive(Clone)]
pub struct Api {
    requests: Requests,
}

fn params(value: &JsValue) -> Result<Value, String> {
    if value.is_undefined() || value.is_null() {
        return Ok(json!({}));
    }
    let text = js_sys::JSON::stringify(value).ok().and_then(|v| v.as_string()).ok_or("params must be JSON serializable")?;
    if text.len() > MAX_JSON_BYTES {
        return Err("params exceed 16 MiB".into());
    }
    serde_json::from_str(&text).map_err(|e| format!("invalid params: {e}"))
}

pub(crate) fn error(message: &str) -> JsValue {
    js_sys::Error::new(message).into()
}

impl Api {
    pub fn install(requests: Requests) -> Result<Self, JsValue> {
        let api = Self { requests };
        let object = js_sys::Object::new();
        let request = api.clone();
        let execute = api.clone();
        let commands = api.clone();
        let inspect = api.clone();
        for (name, function) in [
            (
                "request",
                Closure::<dyn Fn(String, JsValue) -> js_sys::Promise>::new(move |method: String, value: JsValue| {
                    request.request(&method, params(&value))
                })
                .into_js_value(),
            ),
            (
                "execute",
                Closure::<dyn Fn(String, JsValue) -> js_sys::Promise>::new(move |command: String, value: JsValue| {
                    execute.request("engine.execute", params(&value).map(|p| json!({"command": command, "params": p})))
                })
                .into_js_value(),
            ),
            ("commands", Closure::<dyn Fn() -> js_sys::Promise>::new(move || commands.request("engine.commands", Ok(json!({})))).into_js_value()),
            (
                "inspect",
                Closure::<dyn Fn(JsValue) -> js_sys::Promise>::new(move |value: JsValue| inspect.request("document.inspect", params(&value)))
                    .into_js_value(),
            ),
        ] {
            js_sys::Reflect::set(&object, &name.into(), &function)?;
        }
        let window = web_sys::window().ok_or_else(|| error("no window"))?;
        js_sys::Reflect::set(&window, &"solidcraft".into(), &object)?;
        Ok(api)
    }

    fn request(&self, method: &str, params: Result<Value, String>) -> js_sys::Promise {
        let params = match params {
            Ok(params) => params,
            Err(message) => return js_sys::Promise::reject(&error(&message)),
        };
        js_sys::Promise::new(&mut |resolve, reject| {
            let (on_reply, on_refusal) = (reject.clone(), reject);
            let submitted = self.requests.submit(method, params.clone(), move |reply| {
                let result = if reply.get("ok").and_then(Value::as_bool) == Some(true) {
                    js_sys::JSON::parse(&reply.get("result").unwrap_or(&Value::Null).to_string())
                } else {
                    Err(error(reply.get("error").and_then(Value::as_str).unwrap_or("SolidCraft request failed")))
                };
                let _ = match result {
                    Ok(value) => resolve.call1(&JsValue::UNDEFINED, &value),
                    Err(e) => on_reply.call1(&JsValue::UNDEFINED, &e),
                };
            });
            if let Err(message) = submitted {
                let _ = on_refusal.call1(&JsValue::UNDEFINED, &error(&message));
            }
        })
    }
}
