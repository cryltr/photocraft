//! The parent-page bridge: file open/save over `window.postMessage` instead of
//! the browser picker and downloads, active only when the page is framed and
//! loaded with `?host=parent`. The protocol is PdfCraft's host bridge
//! (`apps/pdfcraft-web/src/main.rs`, module `host`), with `photocraft:*`
//! message types; lcdcLive is the first host, embedding the editor from its
//! own origin. Without the flag, nothing here runs and the web shell behaves
//! exactly as upstream.
//!
//! The protocol, parent page <-> editor:
//! - editor -> parent `photocraft:ready` `{ version }`: posted once the app
//!   has started; the parent may then send files.
//! - editor -> parent `photocraft:failed` `{ error }`: startup failed.
//! - editor -> parent `photocraft:save` `{ name, saveAs, bytes }`: posted
//!   instead of a download; the ArrayBuffer is transferred, not copied.
//! - editor -> parent `photocraft:dirty` `{ dirty }`: posted when the
//!   documents' unsaved state changes (repeats are skipped here).
//! - parent -> editor `photocraft:open` `{ name, bytes }`: opens
//!   (name, bytes) as a document, through the same inbox drag-and-drop uses.
//!
//! Only the parent window on the app's own origin is heard or answered.

/// The query flag that turns the bridge on (`?host=parent`).
pub fn host_active(query: &str) -> bool {
    query.contains("host=parent")
}

/// One message type, namespaced with the app slug.
pub fn message_type(kind: &str) -> String {
    format!("photocraft:{kind}")
}

#[cfg(target_arch = "wasm32")]
pub use bridge::Bridge;

#[cfg(target_arch = "wasm32")]
mod bridge {
    use std::sync::{Arc, Mutex};

    use js_sys::Reflect;
    use wasm_bindgen::{JsCast as _, JsValue, closure::Closure};

    use super::message_type;

    type Inbox = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

    /// The parent-page bridge, present only when the page is framed. Clone is
    /// cheap (window handles), so the save and dirty closures each own one.
    #[derive(Clone)]
    pub struct Bridge {
        window: web_sys::Window,
        parent: web_sys::Window,
        origin: String,
    }

    impl Bridge {
        /// None when the page is not framed (then there is no parent to talk
        /// to and the flag is ignored) or the environment is incomplete.
        pub fn new() -> Option<Self> {
            let window = web_sys::window()?;
            let parent = window.parent().ok().flatten()?;
            // An unframed page is its own parent.
            if js_sys::Object::is(&parent, &window) {
                return None;
            }
            let origin = window.location().origin().ok()?;
            Some(Self { window, parent, origin })
        }

        fn post(&self, message: &JsValue, transfer: Option<&js_sys::Array>) {
            let sent = match transfer {
                Some(t) => self.parent.post_message_with_transfer(message, &self.origin, t),
                None => self.parent.post_message(message, &self.origin),
            };
            if let Err(e) = sent {
                log::error!("photocraft host bridge: post failed: {e:?}");
            }
        }

        /// `photocraft:ready { version }`, once the app has started.
        pub fn post_ready(&self) {
            let msg = message("ready");
            set(&msg, "version", &JsValue::from_str(env!("CARGO_PKG_VERSION")));
            self.post(&msg, None);
        }

        /// `photocraft:failed { error }`, when startup fails.
        pub fn post_failed(&self, error: &str) {
            let msg = message("failed");
            set(&msg, "error", &JsValue::from_str(error));
            self.post(&msg, None);
        }

        /// The `write` service for embedded saves: `photocraft:save
        /// { name, saveAs, bytes }`, transferring the buffer. saveAs is false
        /// for now: every web save ends in the same write service and the
        /// shell cannot yet tell Save from Save As there.
        pub fn save_fn(&self) -> Box<dyn FnMut(&str, &[u8]) -> Result<(), String>> {
            let bridge = self.clone();
            Box::new(move |path: &str, bytes: &[u8]| {
                let name = std::path::Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "photocraft".into());
                let array = js_sys::Uint8Array::from(bytes);
                let buffer = array.buffer();
                let msg = message("save");
                set(&msg, "name", &JsValue::from_str(&name));
                set(&msg, "saveAs", &JsValue::from_bool(false));
                set(&msg, "bytes", &buffer);
                bridge.post(&msg, Some(&js_sys::Array::of1(&buffer)));
                Ok(())
            })
        }

        /// A per-frame callable that posts `photocraft:dirty { dirty }` only
        /// when the value changes.
        pub fn dirty_fn(&self) -> Box<dyn FnMut(bool)> {
            let bridge = self.clone();
            let mut last: Option<bool> = None;
            Box::new(move |dirty: bool| {
                if last == Some(dirty) {
                    return;
                }
                last = Some(dirty);
                let msg = message("dirty");
                set(&msg, "dirty", &JsValue::from_bool(dirty));
                bridge.post(&msg, None);
            })
        }

        /// Listens for `photocraft:open` from the parent on our own origin and
        /// pushes (name, bytes) into `inbox`, the same path drag-and-drop
        /// uses; the app drains it every frame.
        pub fn listen(&self, inbox: Inbox, ctx: egui::Context) {
            let origin = self.origin.clone();
            let parent = self.parent.clone();
            let cb = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |e: web_sys::MessageEvent| {
                if e.origin() != origin {
                    return;
                }
                let from_parent = e.source().is_some_and(|s| js_sys::Object::is(&s, &parent));
                if !from_parent {
                    return;
                }
                let data = e.data();
                let kind = Reflect::get(&data, &JsValue::from_str("type")).ok().and_then(|v| v.as_string());
                if kind.as_deref() != Some(message_type("open").as_str()) {
                    return;
                }
                let name = Reflect::get(&data, &JsValue::from_str("name")).ok().and_then(|v| v.as_string()).unwrap_or_else(|| "document".into());
                let Ok(bytes) = Reflect::get(&data, &JsValue::from_str("bytes")) else { return };
                // Accept an ArrayBuffer or a Uint8Array.
                let array = if let Some(buffer) = bytes.dyn_ref::<js_sys::ArrayBuffer>() {
                    js_sys::Uint8Array::new(buffer)
                } else if let Some(view) = bytes.dyn_ref::<js_sys::Uint8Array>() {
                    view.clone()
                } else {
                    log::error!("photocraft:open {name}: bytes is neither ArrayBuffer nor Uint8Array");
                    return;
                };
                inbox.lock().unwrap_or_else(|e| e.into_inner()).push((name, array.to_vec()));
                ctx.request_repaint();
            });
            if self.window.add_event_listener_with_callback("message", cb.as_ref().unchecked_ref()).is_ok() {
                cb.forget();
            }
        }
    }

    fn message(kind: &str) -> js_sys::Object {
        let obj = js_sys::Object::new();
        set(&obj, "type", &JsValue::from_str(&message_type(kind)));
        obj
    }

    fn set(obj: &js_sys::Object, key: &str, value: &JsValue) {
        Reflect::set(obj, &JsValue::from_str(key), value).ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_flag_is_exact() {
        assert!(host_active("?host=parent"));
        assert!(host_active("?webgl&host=parent"));
        assert!(!host_active(""));
        assert!(!host_active("?webgl"));
        assert!(!host_active("?host=other"));
    }

    #[test]
    fn message_types_carry_the_app_slug() {
        assert_eq!(message_type("ready"), "photocraft:ready");
        assert_eq!(message_type("save"), "photocraft:save");
        assert_eq!(message_type("dirty"), "photocraft:dirty");
        assert_eq!(message_type("failed"), "photocraft:failed");
        assert_eq!(message_type("open"), "photocraft:open");
    }
}
