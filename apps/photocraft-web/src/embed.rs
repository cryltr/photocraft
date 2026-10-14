//! The lcdcLive embed link: file open/save over `window.postMessage` instead of
//! the browser picker and downloads, active only when the page is loaded with
//! `?embed=lcdclive` (an iframe inside lcdcLive, lcdc.co's internal platform).
//! Without the flag, nothing here runs and the web shell behaves exactly as
//! upstream.
//!
//! The protocol, parent page <-> editor:
//! - parent -> editor `{ type: "hello" }`: announces the parent so its origin is
//!   known before the first save.
//! - parent -> editor `{ type: "open", name, bytes }`: opens (name, bytes) as a
//!   document, through the same inbox drag-and-drop uses.
//! - editor -> parent `{ type: "ready" }`: posted once the app has started.
//! - editor -> parent `{ type: "dirty", value }`: posted when the unsaved state
//!   of the documents changes.
//! - editor -> parent `{ type: "save", name, bytes }`: posted instead of a
//!   download; the ArrayBuffer is transferred, not copied.
//!
//! Only messages from an allowed origin are accepted, and outgoing messages are
//! posted to that origin alone. If no allowed parent origin can be determined, a
//! save fails (so the document is not marked clean) rather than sending bytes
//! anywhere uncertain.

/// The query flag that turns the embed link on.
pub fn embed_active(query: &str) -> bool {
    query.contains("embed=lcdclive")
}

/// Origins the editor will talk to: lcdcLive in production, and localhost on
/// any port for development and the plain test page. Anything else is ignored.
pub fn origin_allowed(origin: &str) -> bool {
    if origin == "https://lcdclive.app" {
        return true;
    }
    for prefix in ["http://localhost:", "http://127.0.0.1:"] {
        if let Some(port) = origin.strip_prefix(prefix)
            && !port.is_empty()
            && port.bytes().all(|b| b.is_ascii_digit())
        {
            return true;
        }
    }
    false
}

/// The origin part of a URL (scheme://host[:port]), or None when it has no
/// authority. Used on `document.referrer` to learn the parent's origin.
pub fn origin_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    if authority.is_empty() {
        return None;
    }
    Some(format!("{scheme}://{authority}"))
}

#[cfg(target_arch = "wasm32")]
pub use glue::{install, post_dirty, post_ready, post_save};

#[cfg(target_arch = "wasm32")]
mod glue {
    use std::cell::RefCell;
    use std::sync::{Arc, Mutex};

    use js_sys::Reflect;
    use wasm_bindgen::{JsCast as _, JsValue, closure::Closure};

    use super::{origin_allowed, origin_of};

    type Inbox = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

    thread_local! {
        /// The allowed parent origin, once determined. Wasm is single threaded.
        static PARENT_ORIGIN: RefCell<Option<String>> = const { RefCell::new(None) };
    }

    fn remember_origin(origin: &str) {
        if origin_allowed(origin) {
            PARENT_ORIGIN.with(|o| *o.borrow_mut() = Some(origin.to_string()));
        }
    }

    /// The parent's origin: read directly when the parent is same-origin (the
    /// production case, where lcdcLive serves the editor from its own origin),
    /// from `document.referrer` otherwise, and kept up to date by every
    /// accepted inbound message.
    fn detect_origin() {
        let Some(window) = web_sys::window() else { return };
        if let Ok(Some(parent)) = window.parent() {
            // Same-origin parents expose location.origin; cross-origin access
            // throws, which Reflect turns into Err and we fall through.
            if let Ok(location) = Reflect::get(&parent, &JsValue::from_str("location"))
                && let Ok(origin) = Reflect::get(&location, &JsValue::from_str("origin"))
                && let Some(origin) = origin.as_string()
            {
                remember_origin(&origin);
                return;
            }
        }
        if let Some(document) = window.document()
            && let Some(origin) = origin_of(&document.referrer())
        {
            remember_origin(&origin);
        }
    }

    /// Starts the embed link: determines the parent origin and listens for
    /// `hello` and `open` messages. Opened files go into `inbox`, the same path
    /// drag-and-drop uses; the app drains it every frame.
    pub fn install(inbox: Inbox, ctx: egui::Context) {
        detect_origin();
        let Some(window) = web_sys::window() else { return };
        let cb = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |e: web_sys::MessageEvent| {
            let origin = e.origin();
            if !origin_allowed(&origin) {
                return;
            }
            remember_origin(&origin);
            let data = e.data();
            let kind = Reflect::get(&data, &JsValue::from_str("type")).ok().and_then(|v| v.as_string());
            if kind.as_deref() != Some("open") {
                return; // "hello" and anything else: origin capture only.
            }
            let name = Reflect::get(&data, &JsValue::from_str("name"))
                .ok()
                .and_then(|v| v.as_string())
                .unwrap_or_else(|| "lcdcLive file".into());
            let Ok(bytes) = Reflect::get(&data, &JsValue::from_str("bytes")) else { return };
            // Accept an ArrayBuffer or a Uint8Array.
            let array = if let Some(buffer) = bytes.dyn_ref::<js_sys::ArrayBuffer>() {
                js_sys::Uint8Array::new(buffer)
            } else if let Some(view) = bytes.dyn_ref::<js_sys::Uint8Array>() {
                view.clone()
            } else {
                log::error!("embed open {name}: bytes is neither ArrayBuffer nor Uint8Array");
                return;
            };
            inbox.lock().unwrap_or_else(|e| e.into_inner()).push((name, array.to_vec()));
            ctx.request_repaint();
        });
        if window.add_event_listener_with_callback("message", cb.as_ref().unchecked_ref()).is_ok() {
            cb.forget();
        }
    }

    /// Posts `message` to the parent window at the known allowed origin.
    fn post(message: &JsValue, transfer: Option<&js_sys::Array>) -> Result<(), String> {
        let origin = PARENT_ORIGIN.with(|o| o.borrow().clone()).ok_or("no allowed lcdcLive parent origin")?;
        let parent = web_sys::window()
            .and_then(|w| w.parent().ok().flatten())
            .ok_or("no parent window")?;
        match transfer {
            Some(t) => parent.post_message_with_transfer(message, &origin, t),
            None => parent.post_message(message, &origin),
        }
        .map_err(|e| format!("{e:?}"))
    }

    fn message(kind: &str) -> js_sys::Object {
        let obj = js_sys::Object::new();
        Reflect::set(&obj, &JsValue::from_str("type"), &JsValue::from_str(kind)).ok();
        obj
    }

    /// `{ type: "ready" }`, once the app has started.
    pub fn post_ready() {
        if let Err(e) = post(&message("ready"), None) {
            log::error!("embed ready: {e}");
        }
    }

    /// `{ type: "dirty", value }`, when the documents' unsaved state changes.
    pub fn post_dirty(value: bool) {
        let msg = message("dirty");
        Reflect::set(&msg, &JsValue::from_str("value"), &JsValue::from_bool(value)).ok();
        if let Err(e) = post(&msg, None) {
            log::error!("embed dirty: {e}");
        }
    }

    /// `{ type: "save", name, bytes }` instead of a download, transferring the
    /// buffer. An Err leaves the document marked unsaved, exactly like a failed
    /// download would.
    pub fn post_save(path: &str, bytes: &[u8]) -> Result<(), String> {
        let name = std::path::Path::new(path)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "photocraft".into());
        let array = js_sys::Uint8Array::from(bytes);
        let buffer = array.buffer();
        let msg = message("save");
        Reflect::set(&msg, &JsValue::from_str("name"), &JsValue::from_str(&name)).map_err(|e| format!("{e:?}"))?;
        Reflect::set(&msg, &JsValue::from_str("bytes"), &buffer).map_err(|e| format!("{e:?}"))?;
        post(&msg, Some(&js_sys::Array::of1(&buffer)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_flag_is_exact() {
        assert!(embed_active("?embed=lcdclive"));
        assert!(embed_active("?webgl&embed=lcdclive"));
        assert!(!embed_active(""));
        assert!(!embed_active("?webgl"));
        assert!(!embed_active("?embed=other"));
    }

    #[test]
    fn production_origin_is_allowed() {
        assert!(origin_allowed("https://lcdclive.app"));
    }

    #[test]
    fn localhost_is_allowed_on_any_port_over_http() {
        assert!(origin_allowed("http://localhost:3000"));
        assert!(origin_allowed("http://localhost:8787"));
        assert!(origin_allowed("http://127.0.0.1:8765"));
    }

    #[test]
    fn everything_else_is_refused() {
        assert!(!origin_allowed("https://evil.example"));
        assert!(!origin_allowed("http://lcdclive.app")); // http, not https
        assert!(!origin_allowed("https://lcdclive.app.evil.example"));
        assert!(!origin_allowed("http://localhost")); // no port
        assert!(!origin_allowed("http://localhost:")); // empty port
        assert!(!origin_allowed("http://localhost:3000x")); // junk port
        assert!(!origin_allowed("http://localhost.evil.example:3000"));
        assert!(!origin_allowed(""));
    }

    #[test]
    fn referrer_origins_are_extracted() {
        assert_eq!(origin_of("https://lcdclive.app/app?x=1"), Some("https://lcdclive.app".into()));
        assert_eq!(origin_of("http://localhost:8787/embed-test.html"), Some("http://localhost:8787".into()));
        assert_eq!(origin_of("http://localhost:8787"), Some("http://localhost:8787".into()));
        assert_eq!(origin_of(""), None);
        assert_eq!(origin_of("not a url"), None);
        assert_eq!(origin_of("https://"), None);
    }
}
