//! The browser shell: web `Services`, drag-and-drop, and the eframe web runner.

use deckcraft_engine::Session;
use deckcraft_ui_egui::{Inbox, InboxEvent, InboxFile, Services, SlideApp};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use wasm_bindgen::JsCast as _;

const PRESENTATION_EXTS: &[&str] = &["deckcraft", "pptx", "potx", "ppsx", "txt", "md"];
const PICTURE_EXTS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp", "tif", "tiff"];
const AUDIO_EXTS: &[&str] = &["wav", "mp3", "m4a", "aac", "flac", "ogg", "opus", "aif", "aiff"];
const VIDEO_EXTS: &[&str] = &["mp4", "m4v", "mov", "webm"];
const CANVAS_ID: &str = "deckcraft_canvas";
const LOADING_ID: &str = "deckcraft_loading";

/// Start the browser shell: install logging, locate the canvas, launch the eframe runner,
/// build the sample or blank deck, and replace the loading element when launch finishes.
pub fn start() {
    eframe::WebLogger::init(log::LevelFilter::Info).ok();
    wasm_bindgen_futures::spawn_local(async {
        let Some(document) = web_sys::window().and_then(|w| w.document()) else {
            log::error!("no document");
            return;
        };
        let Some(canvas) = document.get_element_by_id(CANVAS_ID).and_then(|e| e.dyn_into::<web_sys::HtmlCanvasElement>().ok()) else {
            log::error!("missing <canvas id=\"{CANVAS_ID}\">");
            return;
        };
        let mut options = eframe::WebOptions::default();
        if query().contains("webgl")
            && let eframe::egui_wgpu::WgpuSetup::CreateNew(create) = &mut options.wgpu_options.wgpu_setup
        {
            create.instance_descriptor.backends = eframe::wgpu::Backends::GL;
        }
        let result = eframe::WebRunner::new()
            .start(
                canvas,
                options,
                Box::new(move |cc| {
                    if let Some(rs) = &cc.wgpu_render_state {
                        log::info!("deckcraft-web {}: wgpu backend {:?}", env!("CARGO_PKG_VERSION"), rs.adapter.get_info().backend);
                    }
                    deckcraft_ui_egui::install_ime_pass_hooks(&cc.egui_ctx);
                    let inbox: Inbox = Inbox::default();
                    let mut app = SlideApp::new(Session::new(), services(inbox.clone(), cc.egui_ctx.clone()));
                    let q = query();
                    if !q.contains("blank") {
                        if let Err(e) = deckcraft_engine::sample::open_sample(&mut app.session) {
                            log::error!("sample deck: {e}");
                        }
                    } else if let Err(e) = app.run("file.new", serde_json::json!({})) {
                        log::error!("new presentation: {e}");
                    }
                    if q.contains("show") {
                        app.start_show(0, false);
                    }
                    let manna = listen_manna(cc.egui_ctx.clone());
                    tell_manna(&serde_json::json!({ "deckcraft": "ready", "version": env!("CARGO_PKG_VERSION") }));
                    Ok(Box::new(WebShell { app, inbox, manna, pending: None, told: String::new(), quiet: false }))
                }),
            )
            .await;
        if let Some(el) = document.get_element_by_id(LOADING_ID) {
            match result {
                Ok(()) => el.remove(),
                Err(e) => el.set_inner_html(&format!("<p>DeckCraft failed to start: {e:?}</p><p>A browser with WebGPU or WebGL2 is required.</p>")),
            }
        }
    });
}

fn query() -> String {
    web_sys::window().and_then(|w| w.location().search().ok()).unwrap_or_default()
}

/// Wraps the app to read dropped files asynchronously (browsers can't read them synchronously)
/// and feed them through the inbox.
struct WebShell {
    app: SlideApp,
    inbox: Inbox,
    /// Messages from Manna, read on the next logic pass.
    manna: Rc<RefCell<VecDeque<MannaMsg>>>,
    /// A presentation Manna sent and is opening: the active document before it, and the slide
    /// to start the show at once it has opened.
    pending: Option<(Option<u64>, usize)>,
    /// The last state told to Manna, so it hears only changes.
    told: String,
    /// A show Manna started: its on-screen tools stay hidden.
    quiet: bool,
}

/// Manna (Manna's copy of DeckCraft, delosriosmedia/deckcraft): the page that frames this one, on
/// the same origin, sends it `{deckcraft: "open", name, bytes, from}` (open a presentation and
/// start the show at `from`), `{deckcraft: "key", key}` (a key of the show, by its egui name:
/// "ArrowRight", "ArrowLeft", "Home"…), `{deckcraft: "goto", slide}` and `{deckcraft: "end"}`.
/// This page tells it `{deckcraft: "ready", version}` once and `{deckcraft: "state", slide, step,
/// ended, slides}` whenever the show moves (`slide` null while there is no show).
enum MannaMsg {
    Open { name: String, bytes: Vec<u8>, from: usize },
    Key(egui::Key),
    Goto(usize),
    End,
}

fn listen_manna(ctx: egui::Context) -> Rc<RefCell<VecDeque<MannaMsg>>> {
    let queue = Rc::new(RefCell::new(VecDeque::new()));
    let Some(window) = web_sys::window() else { return queue };
    let origin = window.location().origin().unwrap_or_default();
    let q = queue.clone();
    let on = wasm_bindgen::closure::Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |e: web_sys::MessageEvent| {
        if e.origin() != origin {
            return;
        }
        let data = e.data();
        let get = |k: &str| js_sys::Reflect::get(&data, &k.into()).ok().filter(|v| !v.is_undefined() && !v.is_null());
        let number = |k: &str| get(k).and_then(|v| v.as_f64()).unwrap_or(0.0).max(0.0) as usize;
        let msg = match get("deckcraft").and_then(|v| v.as_string()).as_deref() {
            Some("open") => MannaMsg::Open {
                name: get("name").and_then(|v| v.as_string()).unwrap_or_else(|| "manna.pptx".into()),
                bytes: get("bytes").map(|b| js_sys::Uint8Array::new(&b).to_vec()).unwrap_or_default(),
                from: number("from"),
            },
            Some("key") => match get("key").and_then(|v| v.as_string()).and_then(|k| egui::Key::from_name(&k)) {
                Some(key) => MannaMsg::Key(key),
                None => return,
            },
            Some("goto") => MannaMsg::Goto(number("slide")),
            Some("end") => MannaMsg::End,
            _ => return,
        };
        q.borrow_mut().push_back(msg);
        ctx.request_repaint();
    });
    window.add_event_listener_with_callback("message", on.as_ref().unchecked_ref()).ok();
    on.forget();
    queue
}

fn tell_manna(v: &serde_json::Value) {
    let Some(window) = web_sys::window() else { return };
    let (Ok(Some(parent)), Ok(origin)) = (window.parent(), window.location().origin()) else { return };
    if let Ok(msg) = js_sys::JSON::parse(&v.to_string()) {
        parent.post_message(&msg, &origin).ok();
    }
}

impl WebShell {
    /// Run Manna's messages before the app's own pass.
    fn manna_in(&mut self) {
        let msgs: Vec<MannaMsg> = self.manna.borrow_mut().drain(..).collect();
        for msg in msgs {
            match msg {
                MannaMsg::Open { name, bytes, from } => {
                    let before = self.app.session.active().map(|d| d.uid);
                    let id = self.app.begin_external_file("drop");
                    push_inbox(&self.inbox, InboxEvent::File(InboxFile { id, name, bytes }));
                    // The projector shows the audience's view, the slide alone, not the presenter's.
                    self.app.ui.presenter_view = false;
                    self.pending = Some((before, from));
                }
                MannaMsg::Key(key) if self.app.show.as_ref().is_some_and(|s| !s.ended) => self.press(key),
                MannaMsg::Key(_) => {}
                MannaMsg::Goto(slide) => {
                    self.app.start_show(slide, false);
                    self.quiet = true;
                }
                // Like the show's own Escape, so whatever is playing stops with it.
                MannaMsg::End if self.app.show.is_some() => self.press(egui::Key::Escape),
                MannaMsg::End => {}
            }
        }
    }

    fn press(&mut self, key: egui::Key) {
        for pressed in [true, false] {
            self.app.queue_input(egui::Event::Key { key, physical_key: None, pressed, repeat: false, modifiers: egui::Modifiers::NONE });
        }
    }

    /// After the app's pass: start the show of a presentation that just opened, and tell Manna
    /// where the show is when it moved.
    fn manna_out(&mut self) {
        if let Some((before, from)) = self.pending
            && self.app.session.active().map(|d| d.uid) != before
        {
            self.pending = None;
            self.app.start_show(from, false);
            self.quiet = true;
        }
        if self.quiet && self.app.show.as_mut().is_none_or(|s| s.quiet()) {
            self.quiet = false;
        }
        let slides = self.app.session.active().map(|d| d.doc.slides.len()).unwrap_or(0);
        let state = match self.app.show.as_ref() {
            Some(s) => serde_json::json!({ "deckcraft": "state", "slide": s.state.slide, "step": s.state.step, "ended": s.ended, "slides": slides }),
            None => serde_json::json!({ "deckcraft": "state", "slide": null, "step": null, "ended": false, "slides": slides }),
        };
        let text = state.to_string();
        if text != self.told {
            tell_manna(&state);
            self.told = text;
        }
    }
}

impl eframe::App for WebShell {
    /// Delegate one eframe logic pass to the wrapped application.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.manna_in();
        self.app.logic(ctx);
        self.manna_out();
    }

    /// Consume browser drops with fresh input, capture the identity before the asynchronous read,
    /// and deliver the id, name, and bytes through the inbox.
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        // Browsers cannot read dropped files synchronously. Consume this pass's drops here, with
        // fresh input, and capture the document/slide before the asynchronous read finishes.
        for f in std::mem::take(&mut raw.dropped_files) {
            let id = self.app.begin_external_file("drop");
            let inbox = self.inbox.clone();
            let ctx = ctx.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let name = f.path().file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "dropped".into());
                match f.bytes_async().await {
                    Ok(bytes) => {
                        push_inbox(&inbox, InboxEvent::File(InboxFile { id, name, bytes }));
                        ctx.request_repaint();
                    }
                    Err(e) => {
                        log::error!("couldn't read dropped file {name}: {e}");
                        push_inbox(&inbox, InboxEvent::Cancelled { id });
                        ctx.request_repaint();
                    }
                }
            });
        }
        self.app.raw_input_hook(ctx, raw);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.app.ui(ui);
    }
}

/// Web services: no file paths, so `pick_open`/`pick_save`/`read` stay `None` and the UI falls
/// back to `open_async` (browser picker → inbox) and `download`.
fn services(inbox: Inbox, ctx: egui::Context) -> Services {
    let open_inbox = inbox.clone();
    Services {
        open_async: Some(Box::new(move |purpose: &str, request: u64| {
            let inbox = open_inbox.clone();
            let ctx = ctx.clone();
            let dialog = match purpose {
                "picture" => rfd::AsyncFileDialog::new().add_filter("Pictures", PICTURE_EXTS),
                "audio" => rfd::AsyncFileDialog::new().add_filter("Audio", AUDIO_EXTS),
                "video" => rfd::AsyncFileDialog::new().add_filter("Video", VIDEO_EXTS),
                _ => rfd::AsyncFileDialog::new().add_filter("Presentations", PRESENTATION_EXTS),
            };
            wasm_bindgen_futures::spawn_local(async move {
                let Some(file) = dialog.pick_file().await else {
                    push_inbox(&inbox, InboxEvent::Cancelled { id: request });
                    ctx.request_repaint();
                    return;
                };
                let bytes = file.read().await;
                push_inbox(&inbox, InboxEvent::File(InboxFile { id: request, name: file.file_name(), bytes }));
                ctx.request_repaint();
            });
        })),
        write: Some(Box::new(|name: &str, bytes: &[u8]| download(name, bytes))),
        download: Some(Box::new(|name: &str, bytes: &[u8]| {
            if let Err(e) = download(name, bytes) {
                log::error!("download of {name} failed: {e}");
            }
        })),
        inbox: Some(inbox),
        ..Default::default()
    }
}

fn push_inbox(inbox: &Inbox, event: InboxEvent) {
    inbox.lock().unwrap_or_else(|e| e.into_inner()).push(event);
}

/// Trigger a browser download of `bytes` named after the last component of `path`.
fn download(path: &str, bytes: &[u8]) -> Result<(), String> {
    let js = |e: wasm_bindgen::JsValue| format!("{e:?}");
    let name = std::path::Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "deckcraft".into());
    let window = web_sys::window().ok_or("no window")?;
    let document = window.document().ok_or("no document")?;
    let parts = js_sys::Array::of1(&js_sys::Uint8Array::from(bytes));
    let opts = web_sys::BlobPropertyBag::new();
    opts.set_type(mime_for(&name));
    let blob = web_sys::Blob::new_with_u8_array_sequence_and_options(&parts, &opts).map_err(js)?;
    let url = web_sys::Url::create_object_url_with_blob(&blob).map_err(js)?;
    let a: web_sys::HtmlAnchorElement = document.create_element("a").map_err(js)?.dyn_into().map_err(|_| "not an anchor")?;
    a.set_href(&url);
    a.set_download(&name);
    a.style().set_property("display", "none").map_err(js)?;
    let body = document.body().ok_or("no body")?;
    body.append_child(&a).map_err(js)?;
    a.click();
    a.remove();
    // Revoke after the click has been dispatched; the download keeps its own reference.
    let revoke = wasm_bindgen::closure::Closure::once_into_js(move || {
        web_sys::Url::revoke_object_url(&url).ok();
    });
    window.set_timeout_with_callback_and_timeout_and_arguments_0(revoke.unchecked_ref(), 10_000).map_err(js)?;
    Ok(())
}

fn mime_for(name: &str) -> &'static str {
    match name.rsplit('.').next().map(str::to_ascii_lowercase).as_deref() {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("pdf") => "application/pdf",
        Some("gif") => "image/gif",
        Some("txt") => "text/plain",
        Some("pptx") => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        Some("deckcraft") => "application/zip",
        _ => "application/octet-stream",
    }
}
