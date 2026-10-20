//! Builder-owned Android shell for PhotoCraft.
//! Platform file operations use Android's Storage Access Framework (SAF). Selected documents
//! are staged inside app-private storage, with a persistent URI mapping used to write saved bytes
//! back through ContentResolver. The upstream PhotoCraft crates remain unchanged.
//! ARTCRAFT_PHOTOCRAFT_SAF_JNI022_FIX_V1

use android_activity::AndroidApp;
use jni::{
    JavaVM, jni_sig, jni_str,
    objects::{JObject, JString, JValue},
};
use photocraft_codecs::{ChannelLayout, EncodeOptions, Image};
use photocraft_doc::Document;
use photocraft_engine::Session;
use photocraft_ui_egui::{
    FileDialogAnswer, FileDialogReply, FileDialogRequest, Inbox, PhotocraftApp, Services,
};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

// Shared with the generated launchers: brings the device's own system fonts into the egui fallback
// chain and makes them visible to the app's own font scanners (see templates/android/system_fonts.rs).
mod system_fonts;

// Shared with the generated launchers: turns Android's soft keyboard into egui IME events (see
// templates/android/ime_bridge.rs). The `mod ime_bridge;` marker also keeps the vendored
// android-activity, which forwards show/hide to this Activity's imeShow/imeHide, in step.
mod ime_bridge;

/// Emitted at startup so a device log (and the packaged `.so` strings) can confirm this shell —
/// including the Android modifier-key fix and immersive fullscreen — is the one that is running.
const ANDROID_SHELL_MARKER: &str = "ARTCRAFT_PHOTOCRAFT_ANDROID_SHELL_V3_SHORTCUT_BAR";

struct AndroidFileBridge {
    app: AndroidApp,
    vm: Arc<JavaVM>,
}

impl AndroidFileBridge {
    fn new(app: AndroidApp) -> Result<Self, String> {
        // AndroidApp owns the live VM pointer for the lifetime of the activity.
        let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr() as *mut _) };
        Ok(Self {
            app,
            vm: Arc::new(vm),
        })
    }

    /// Tell the Activity whether an on-screen Ctrl/Alt is armed, so the soft keyboard commits each
    /// letter immediately (no composing word) and the shell can turn it into a shortcut key press.
    fn set_shortcut_mode(&self, on: bool) {
        let result = self.vm.attach_current_thread(|env| -> jni::errors::Result<()> {
            let activity_raw = self.app.activity_as_ptr() as jni::sys::jobject;
            // SAFETY: see request_open.
            let activity = unsafe { env.as_cast_raw::<JObject>(&activity_raw)? };
            if let Err(error) = env.call_method(
                &*activity,
                jni_str!("imeSetShortcutMode"),
                jni_sig!("(Z)V"),
                &[JValue::Bool(on)],
            ) {
                env.exception_clear();
                return Err(error);
            }
            Ok(())
        });
        if let Err(error) = result {
            log::warn!("shortcut mode unavailable: {error}");
        }
    }

    fn request_open(&self, id: u64, multiple: bool) -> Result<(), String> {
        self.vm
            .attach_current_thread(|env| -> jni::errors::Result<()> {
                // AndroidApp returns an unowned global ref. Do not delete it when this wrapper drops.
                let activity_raw = self.app.activity_as_ptr() as jni::sys::jobject;
                // SAFETY: AndroidApp retains this global Activity reference; as_cast_raw borrows it
                // without taking ownership or attempting to delete the reference.
                let activity = unsafe { env.as_cast_raw::<JObject>(&activity_raw)? };
                env.call_method(
                    &*activity,
                    jni_str!("pcRequestOpen"),
                    jni_sig!("(JZ)V"),
                    &[JValue::Long(id as i64), JValue::Bool(multiple)],
                )?;
                Ok(())
            })
            .map_err(|e| e.to_string())
    }

    fn request_save(&self, id: u64, name: &str) -> Result<(), String> {
        self.vm
            .attach_current_thread(|env| -> jni::errors::Result<()> {
                let activity_raw = self.app.activity_as_ptr() as jni::sys::jobject;
                // SAFETY: AndroidApp retains this global Activity reference; as_cast_raw borrows it
                // without taking ownership or attempting to delete the reference.
                let activity = unsafe { env.as_cast_raw::<JObject>(&activity_raw)? };
                let name = env.new_string(name)?;
                let name_obj = JObject::from(name);
                env.call_method(
                    &*activity,
                    jni_str!("pcRequestSave"),
                    jni_sig!("(JLjava/lang/String;)V"),
                    &[JValue::Long(id as i64), JValue::Object(&name_obj)],
                )?;
                Ok(())
            })
            .map_err(|e| e.to_string())
    }

    fn poll_result(&self) -> Result<Option<String>, String> {
        self.vm
            .attach_current_thread(|env| -> jni::errors::Result<Option<String>> {
                let activity_raw = self.app.activity_as_ptr() as jni::sys::jobject;
                // SAFETY: AndroidApp retains this global Activity reference; as_cast_raw borrows it
                // without taking ownership or attempting to delete the reference.
                let activity = unsafe { env.as_cast_raw::<JObject>(&activity_raw)? };
                let value = env
                    .call_method(
                        &*activity,
                        jni_str!("pcPollPickerResult"),
                        jni_sig!("()Ljava/lang/String;"),
                        &[],
                    )?
                    .l()?;
                if value.is_null() {
                    return Ok(None);
                }
                let value = JString::cast_local(env, value)?;
                let value: String = value.try_to_string(env)?;
                Ok(Some(value))
            })
            .map_err(|e| e.to_string())
    }

    /// Hand back the private path of one file the system asked this app to open (an
    /// `ACTION_VIEW`/`ACTION_SEND` it is registered for), or `None` when nothing is pending. Returns
    /// an error when the Activity has no intent host; the caller then stops asking.
    fn intent_poll(&self) -> Result<Option<String>, String> {
        self.vm
            .attach_current_thread(|env| -> jni::errors::Result<Option<String>> {
                let activity_raw = self.app.activity_as_ptr() as jni::sys::jobject;
                // SAFETY: AndroidApp retains this global Activity reference; as_cast_raw borrows it
                // without taking ownership or attempting to delete the reference.
                let activity = unsafe { env.as_cast_raw::<JObject>(&activity_raw)? };
                let value = match env.call_method(
                    &*activity,
                    jni_str!("intentPoll"),
                    jni_sig!("()Ljava/lang/String;"),
                    &[],
                ) {
                    Ok(value) => value.l()?,
                    Err(error) => {
                        // A missing `intentPoll` leaves a pending NoSuchMethodError on the shared
                        // JNI thread (the picker bridge uses it too); clear it before returning.
                        env.exception_clear();
                        return Err(error);
                    }
                };
                if value.is_null() {
                    return Ok(None);
                }
                let value = JString::cast_local(env, value)?;
                let value: String = value.try_to_string(env)?;
                Ok(Some(value))
            })
            .map_err(|e| e.to_string())
    }

    fn files_dir(&self) -> Result<PathBuf, String> {
        self.vm
            .attach_current_thread(|env| -> jni::errors::Result<PathBuf> {
                let activity_raw = self.app.activity_as_ptr() as jni::sys::jobject;
                // SAFETY: AndroidApp retains this global Activity reference; as_cast_raw borrows it
                // without taking ownership or attempting to delete the reference.
                let activity = unsafe { env.as_cast_raw::<JObject>(&activity_raw)? };
                let value = env
                    .call_method(
                        &*activity,
                        jni_str!("pcFilesDir"),
                        jni_sig!("()Ljava/lang/String;"),
                        &[],
                    )?
                    .l()?;
                let value = JString::cast_local(env, value)?;
                let value: String = value.try_to_string(env)?;
                Ok(PathBuf::from(value))
            })
            .map_err(|e| e.to_string())
    }

    /// Returns NONE for ordinary private paths, OK after committing a selected SAF destination,
    /// or ERROR:<message> if the Android provider refused the write.
    fn commit_if_mapped(&self, path: &str) -> Result<String, String> {
        self.vm
            .attach_current_thread(|env| -> jni::errors::Result<String> {
                let activity_raw = self.app.activity_as_ptr() as jni::sys::jobject;
                // SAFETY: AndroidApp retains this global Activity reference; as_cast_raw borrows it
                // without taking ownership or attempting to delete the reference.
                let activity = unsafe { env.as_cast_raw::<JObject>(&activity_raw)? };
                let path = env.new_string(path)?;
                let path_obj = JObject::from(path);
                let value = env
                    .call_method(
                        &*activity,
                        jni_str!("pcCommitIfMapped"),
                        jni_sig!("(Ljava/lang/String;)Ljava/lang/String;"),
                        &[JValue::Object(&path_obj)],
                    )?
                    .l()?;
                let value = JString::cast_local(env, value)?;
                let value: String = value.try_to_string(env)?;
                Ok(value)
            })
            .map_err(|e| e.to_string())
    }
}

#[derive(Default)]
struct PickerState {
    next_id: AtomicU64,
    replies: Mutex<HashMap<u64, FileDialogReply>>,
}

impl PickerState {
    fn launch(
        &self,
        bridge: &AndroidFileBridge,
        request: FileDialogRequest,
        reply: FileDialogReply,
    ) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        let launch_result = match request {
            FileDialogRequest::Open { multiple, .. } => {
                self.replies
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(id, reply);
                bridge.request_open(id, multiple)
            }
            FileDialogRequest::Save { suggested } => {
                let name = Path::new(&suggested)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "Untitled.pcraft".to_string());
                self.replies
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(id, reply);
                bridge.request_save(id, &name)
            }
            FileDialogRequest::Folder { .. } => {
                reply.send(Some(FileDialogAnswer::Unavailable(
                    "Choosing folders is not supported by this Android file picker yet. Use Open or Save As for individual files.".into(),
                )));
                return;
            }
        };
        if let Err(error) = launch_result {
            if let Some(reply) = self
                .replies
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id)
            {
                reply.send(Some(FileDialogAnswer::Unavailable(format!(
                    "Could not start Android file picker: {error}"
                ))));
            }
        }
    }

    fn poll(&self, bridge: &AndroidFileBridge, ctx: &egui::Context) {
        // Each result is one short JSON object queued by PhotoCraftActivity after picker I/O ends.
        for _ in 0..8 {
            let raw = match bridge.poll_result() {
                Ok(Some(raw)) => raw,
                Ok(None) => break,
                Err(error) => {
                    log::error!("Android file picker polling failed: {error}");
                    break;
                }
            };
            let value: serde_json::Value = match serde_json::from_str(&raw) {
                Ok(value) => value,
                Err(error) => {
                    log::error!("Invalid Android picker result: {error}: {raw}");
                    continue;
                }
            };
            let Some(id) = value.get("id").and_then(serde_json::Value::as_u64) else {
                log::error!("Android picker result has no request id: {raw}");
                continue;
            };
            let Some(reply) = self
                .replies
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id)
            else {
                log::warn!("Ignoring stale Android picker reply {id}");
                continue;
            };
            let status = value
                .get("status")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("error");
            let kind = value
                .get("kind")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let answer = match status {
                "cancel" => None,
                "ok" if kind == "open" => {
                    let paths = value
                        .get("paths")
                        .and_then(serde_json::Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(serde_json::Value::as_str)
                        .map(str::to_owned)
                        .collect::<Vec<_>>();
                    (!paths.is_empty()).then_some(FileDialogAnswer::Paths(paths))
                }
                "ok" if kind == "save" => value
                    .get("path")
                    .and_then(serde_json::Value::as_str)
                    .map(|path| FileDialogAnswer::SaveTo(path.to_owned())),
                _ => Some(FileDialogAnswer::Unavailable(
                    value
                        .get("message")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("Android file operation failed")
                        .to_owned(),
                )),
            };
            reply.send(answer);
            ctx.request_repaint();
        }
    }
}

fn write_mobile(bridge: &AndroidFileBridge, path: &str, bytes: &[u8]) -> Result<(), String> {
    let target = Path::new(path);
    if let Some(parent) = target.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Could not create {}: {e}", parent.display()))?;
    }
    let name = target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("photocraft-output");
    let parent = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let temporary = parent.join(format!(".{name}.tmp-{}", std::process::id()));
    fs::write(&temporary, bytes)
        .map_err(|e| format!("Could not write {}: {e}", temporary.display()))?;
    if let Err(error) = fs::rename(&temporary, target) {
        let _ = fs::remove_file(&temporary);
        return Err(format!(
            "Could not finish writing {}: {error}",
            target.display()
        ));
    }
    match bridge.commit_if_mapped(path)? {
        state if state == "NONE" || state == "OK" => Ok(()),
        state if state.starts_with("ERROR:") => Err(state.trim_start_matches("ERROR:").to_owned()),
        state => Err(format!("Unexpected Android save result: {state}")),
    }
}

fn services(bridge: AndroidFileBridge, picker: Arc<PickerState>, prefs_path: PathBuf, inbox: Inbox) -> Services {
    let write_bridge = bridge.clone_for_services();
    let save_bridge = bridge.clone_for_services();
    let prefs_bridge = bridge.clone_for_services();
    let load_prefs_path = prefs_path.clone();
    Services {
        // Files the system asked us to open (ACTION_VIEW/ACTION_SEND) are queued here and drained
        // by the app's own per-frame inbox, exactly like a document chosen in the picker.
        inbox: Some(inbox),
        import: Some(Box::new(
            |name: &str, bytes: &[u8], max_svg_group_depth: usize| {
                photocraft_io::import_with_svg_group_depth(name, bytes, max_svg_group_depth)
                    .map(|r| (r.document, r.warnings))
                    .map_err(|e| e.to_string())
            },
        )),
        export: Some(Box::new(
            |doc: &Document, path: &str, settings: &photocraft_ui_egui::ExportSettings| {
                let mut opts = photocraft_io::ExportOptions::default();
                if let Some(q) = settings.jpeg_quality {
                    opts.encode.jpeg_quality = q;
                }
                opts.encode.webp_lossless = settings.webp_lossless;
                if let Some(q) = settings.webp_quality {
                    opts.encode.webp_quality = q;
                }
                opts.tiff_layers = settings.tiff_layers;
                opts.xmp = if settings.xmp_all {
                    photocraft_io::XmpEmbed::All
                } else {
                    photocraft_io::XmpEmbed::None
                };
                opts.tga_bits = settings.tga_bits;
                opts.image_previews = settings.image_previews;
                opts.psd_maximize_compatibility = settings.psd_maximize_compatibility;
                photocraft_io::export(doc, path, &opts)
                    .map(|r| (r.bytes, r.warnings))
                    .map_err(|e| e.to_string())
            },
        )),
        // Native .pcraft Save uses save_file rather than export. Route it through the same SAF-aware writer.
        save_file: Some(std::sync::Arc::new(
            move |doc: &Document,
                  path: &str,
                  settings: &photocraft_ui_egui::ExportSettings,
                  ctl: &photocraft_ui_egui::jobs_ui::SaveCtl| {
                ctl.progress(0.0, "Encoding");
                let mut opts = photocraft_io::ExportOptions::default();
                if let Some(q) = settings.jpeg_quality {
                    opts.encode.jpeg_quality = q;
                }
                opts.encode.webp_lossless = settings.webp_lossless;
                if let Some(q) = settings.webp_quality {
                    opts.encode.webp_quality = q;
                }
                opts.tiff_layers = settings.tiff_layers;
                opts.xmp = if settings.xmp_all {
                    photocraft_io::XmpEmbed::All
                } else {
                    photocraft_io::XmpEmbed::None
                };
                opts.tga_bits = settings.tga_bits;
                opts.image_previews = settings.image_previews;
                opts.psd_maximize_compatibility = settings.psd_maximize_compatibility;
                let result = photocraft_io::export(doc, path, &opts).map_err(|e| e.to_string())?;
                ctl.commit()?;
                ctl.progress(0.85, "Writing");
                write_mobile(&save_bridge, path, &result.bytes)?;
                ctl.progress(1.0, "");
                Ok(result.warnings)
            },
        )),
        file_dialog: Some(Box::new(move |request, _parent, reply| {
            picker.launch(&bridge, request, reply);
        })),
        write: Some(Box::new(move |path: &str, bytes: &[u8]| {
            write_mobile(&write_bridge, path, bytes)
        })),
        load_prefs: Some(Box::new(move || fs::read_to_string(&load_prefs_path).ok())),
        save_prefs: Some(Box::new(move |text: &str| {
            write_mobile(
                &prefs_bridge,
                &prefs_path.to_string_lossy(),
                text.as_bytes(),
            )
        })),
        encode_png: Some(Box::new(|w, h, rgba| {
            let img = Image::from_u8(w, h, ChannelLayout::Rgba, rgba.to_vec())
                .map_err(|e| e.to_string())?;
            photocraft_codecs::encode(
                &img,
                photocraft_codecs::Format::Png,
                &EncodeOptions::default(),
            )
            .map_err(|e| e.to_string())
        })),
        ..Services::default()
    }
}

// Keep the JavaVM and the AndroidApp cheap to clone into platform-service closures.
impl AndroidFileBridge {
    fn clone_for_services(&self) -> Self {
        Self {
            app: self.app.clone(),
            vm: self.vm.clone(),
        }
    }
}

/// Reconstruct egui's modifier state on Android.
///
/// winit's Android backend delivers `KeyboardInput` events but never emits `ModifiersChanged`
/// (it discards the `KeyEvent`'s meta state), so egui's modifier state stays empty and
/// `Ctrl`/`Alt` shortcuts are ignored while plain keys continue to work. Physical modifier keys
/// do arrive as `Event::Key` events (`Key::ControlLeft`, `Key::AltLeft`, ...), so we track them
/// and stamp the resulting state onto this frame's events. This mirrors `egui-winit`'s
/// `ModifiersChanged` handling and suppresses the text character that would otherwise be emitted
/// alongside a Ctrl/Command shortcut.
fn apply_android_modifiers(state: &mut egui::Modifiers, raw: &mut egui::RawInput) {
    let before = *state;
    let mut kept: Vec<egui::Event> = Vec::with_capacity(raw.events.len());
    for mut event in raw.events.drain(..) {
        match &mut event {
            egui::Event::Key {
                key,
                physical_key,
                pressed,
                modifiers,
                ..
            } => {
                let tracked = physical_key.unwrap_or(*key);
                match tracked {
                    egui::Key::ShiftLeft | egui::Key::ShiftRight => state.shift = *pressed,
                    egui::Key::ControlLeft | egui::Key::ControlRight => state.ctrl = *pressed,
                    egui::Key::AltLeft | egui::Key::AltRight => state.alt = *pressed,
                    _ => {}
                }
                state.mac_cmd = false;
                state.command = state.ctrl;
                *modifiers = *state;
                kept.push(event);
            }
            egui::Event::PointerButton { modifiers, .. } => {
                *modifiers = *state;
                kept.push(event);
            }
            egui::Event::MouseWheel { modifiers, .. } => {
                *modifiers = *state;
                kept.push(event);
            }
            egui::Event::Text(_) if state.ctrl || state.command || state.mac_cmd => {
                // Suppress text emitted with a modifier chord.
            }
            egui::Event::WindowFocused(false) => {
                *state = egui::Modifiers::default();
                kept.push(event);
            }
            _ => kept.push(event),
        }
    }
    raw.events = kept;
    if *state != before {
        // `RawInput` has no modifier field in this egui version: `InputState.modifiers` is driven
        // purely by this event (plus its own per-event copies), so it must be emitted on change.
        raw.events.push(egui::Event::ModifiersChanged(*state));
    }
}


// ---------------------------------------------------------------------------------------------
// On-screen shortcut bar: sticky Ctrl / Shift / Alt plus Esc, Tab, Del and the arrow keys, for
// devices with only the soft keyboard. Tap a modifier once = applies to the next key or click;
// tap twice = locked; tap again = off. With Ctrl/Alt armed, a Latin letter typed on the soft
// keyboard becomes a real shortcut key press (Ctrl+Z, Ctrl+Shift+S, Alt+...).
// ---------------------------------------------------------------------------------------------
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Sticky {
    #[default]
    Off,
    Once,
    Locked,
}

impl Sticky {
    fn on(self) -> bool {
        self != Sticky::Off
    }
    fn tap(self) -> Self {
        match self {
            Sticky::Off => Sticky::Once,
            Sticky::Once => Sticky::Locked,
            Sticky::Locked => Sticky::Off,
        }
    }
}

#[derive(Default)]
struct ShortcutBar {
    open: bool,
    ctrl: Sticky,
    shift: Sticky,
    alt: Sticky,
    /// Keys tapped on the bar; injected as key presses at the start of the next frame.
    pending: Vec<egui::Key>,
    /// Where the bar was drawn last frame, so taps on it do not use up a one-shot modifier.
    rect: Option<egui::Rect>,
    /// Last Ctrl/Alt-armed state sent to the Activity.
    java_mode: bool,
    /// Soft keyboard requested from the bar (true = show, false = hide); handled next frame.
    kb_request: Option<bool>,
    kb_on: bool,
    /// One-shot "right-click" armed from the bar: the next tap becomes a secondary click.
    rc_once: bool,
    /// Long-press right-click is on unless switched off from the bar.
    long_press_off: bool,
    /// Primary press being watched for a long press: where and since when.
    press: Option<(egui::Pos2, std::time::Instant)>,
    /// A long press already fired for the current touch (its real release is dropped).
    press_fired: bool,
    /// The current primary press was turned into a secondary one; convert its release too.
    rc_release: bool,
    /// Last modifier state announced to egui.
    last: egui::Modifiers,
}

fn or_mods(a: egui::Modifiers, b: egui::Modifiers) -> egui::Modifiers {
    egui::Modifiers {
        alt: a.alt || b.alt,
        ctrl: a.ctrl || b.ctrl,
        shift: a.shift || b.shift,
        mac_cmd: false,
        command: a.ctrl || b.ctrl || a.command || b.command,
    }
}

impl ShortcutBar {
    fn virtual_mods(&self) -> egui::Modifiers {
        let ctrl = self.ctrl.on();
        egui::Modifiers {
            alt: self.alt.on(),
            ctrl,
            shift: self.shift.on(),
            mac_cmd: false,
            command: ctrl,
        }
    }

    fn release_once(&mut self) {
        for m in [&mut self.ctrl, &mut self.shift, &mut self.alt] {
            if *m == Sticky::Once {
                *m = Sticky::Off;
            }
        }
    }

    fn ui(&mut self, ctx: &egui::Context, screen: egui::Rect) {
        let default_pos = egui::pos2(screen.right() - 52.0, screen.bottom() - 64.0);
        let open = self.open;
        let mut toggle = false;
        let area = egui::Area::new(egui::Id::new("photocraft_android_shortcut_bar"))
            .order(egui::Order::Foreground)
            .default_pos(if open {
                egui::pos2(screen.center().x - 190.0, screen.bottom() - 64.0)
            } else {
                default_pos
            })
            .movable(true)
            .constrain(true)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        if !open {
                            if ui.button("⌨").clicked() {
                                toggle = true;
                            }
                            return;
                        }
                        let mut modifier = |ui: &mut egui::Ui, label: &str, state: &mut Sticky| {
                            let text = match *state {
                                Sticky::Off => label.to_string(),
                                Sticky::Once => format!("{label}•"),
                                Sticky::Locked => format!("{label}🔒"),
                            };
                            if ui.selectable_label(state.on(), text).clicked() {
                                *state = state.tap();
                            }
                        };
                        modifier(ui, "Ctrl", &mut self.ctrl);
                        modifier(ui, "Shift", &mut self.shift);
                        modifier(ui, "Alt", &mut self.alt);
                        ui.separator();
                        let kb_label = if self.kb_on { "Kb ▾" } else { "Kb ▴" };
                        if ui.button(kb_label).clicked() {
                            self.kb_on = !self.kb_on;
                            self.kb_request = Some(self.kb_on);
                        }
                        if ui.selectable_label(self.rc_once, "RC").clicked() {
                            self.rc_once = !self.rc_once;
                        }
                        if ui
                            .selectable_label(!self.long_press_off, "LP")
                            .clicked()
                        {
                            self.long_press_off = !self.long_press_off;
                            self.press = None;
                        }
                        ui.separator();
                        for (label, key) in [
                            ("Esc", egui::Key::Escape),
                            ("Tab", egui::Key::Tab),
                            ("Del", egui::Key::Delete),
                            ("←", egui::Key::ArrowLeft),
                            ("↑", egui::Key::ArrowUp),
                            ("↓", egui::Key::ArrowDown),
                            ("→", egui::Key::ArrowRight),
                        ] {
                            if ui.button(label).clicked() {
                                self.pending.push(key);
                            }
                        }
                        ui.separator();
                        if ui.button("✕").clicked() {
                            toggle = true;
                        }
                    });
                });
            });
        self.rect = Some(area.response.rect);
        if toggle {
            self.open = !self.open;
            if !self.open {
                if self.kb_on {
                    self.kb_on = false;
                    self.kb_request = Some(false);
                }
                self.ctrl = Sticky::Off;
                self.shift = Sticky::Off;
                self.alt = Sticky::Off;
            }
        }
        if self.kb_request.is_some() || self.press.is_some() || !self.pending.is_empty() || self.ctrl.on() || self.shift.on() || self.alt.on() {
            ctx.request_repaint();
        }
    }
}

/// The egui key a typed character stands for, for shortcut chords. `None` for anything that is not
/// a Latin letter, digit or common shortcut punctuation (e.g. Arabic letters), which then types
/// normally.
fn key_for_char(c: char) -> Option<egui::Key> {
    if c.is_ascii_alphanumeric() {
        return egui::Key::from_name(&c.to_ascii_uppercase().to_string());
    }
    match c {
        '-' | '_' => Some(egui::Key::Minus),
        '+' => Some(egui::Key::Plus),
        '=' => Some(egui::Key::Equals),
        '[' | '{' => Some(egui::Key::OpenBracket),
        ']' | '}' => Some(egui::Key::CloseBracket),
        ',' => Some(egui::Key::Comma),
        '.' => Some(egui::Key::Period),
        '/' => Some(egui::Key::Slash),
        _ => None,
    }
}

fn key_event(key: egui::Key, pressed: bool, modifiers: egui::Modifiers) -> egui::Event {
    egui::Event::Key {
        key,
        physical_key: Some(key),
        pressed,
        repeat: false,
        modifiers,
    }
}

fn is_modifier_key(key: egui::Key) -> bool {
    matches!(
        key,
        egui::Key::ShiftLeft
            | egui::Key::ShiftRight
            | egui::Key::ControlLeft
            | egui::Key::ControlRight
            | egui::Key::AltLeft
            | egui::Key::AltRight
    )
}

/// Merge the on-screen modifiers into this frame's events (run after `apply_android_modifiers`,
/// with the physical keyboard's state in `physical`).
fn apply_virtual_modifiers(bar: &mut ShortcutBar, physical: egui::Modifiers, raw: &mut egui::RawInput) {
    let virt = bar.virtual_mods();
    let had_mod_event = raw
        .events
        .iter()
        .any(|e| matches!(e, egui::Event::ModifiersChanged(_)));
    let mut consumed = false;
    let mut out: Vec<egui::Event> = Vec::with_capacity(raw.events.len() + 8);

    let m = or_mods(physical, virt);
    for key in bar.pending.drain(..) {
        out.push(key_event(key, true, m));
        out.push(key_event(key, false, m));
        consumed = true;
    }

    for mut event in raw.events.drain(..) {
        if let egui::Event::Ime(egui::ImeEvent::Commit(text)) = &event {
            if virt.ctrl || virt.alt {
                let mut it = text.chars();
                if let (Some(c), None) = (it.next(), it.next()) {
                    if let Some(key) = key_for_char(c) {
                        out.push(key_event(key, true, m));
                        if m.ctrl && virt.ctrl {
                            match key {
                                egui::Key::C => out.push(egui::Event::Copy),
                                egui::Key::X => out.push(egui::Event::Cut),
                                _ => {}
                            }
                        }
                        out.push(key_event(key, false, m));
                        consumed = true;
                        continue;
                    }
                }
            } else if virt.shift {
                let upper = text.to_uppercase();
                out.push(egui::Event::Ime(egui::ImeEvent::Commit(upper)));
                consumed = true;
                continue;
            }
            out.push(event);
            continue;
        }
        match &mut event {
            egui::Event::Key {
                key,
                physical_key,
                pressed,
                modifiers,
                ..
            } => {
                let tracked = physical_key.unwrap_or(*key);
                if !is_modifier_key(tracked) {
                    *modifiers = or_mods(*modifiers, virt);
                    if *pressed && (virt.ctrl || virt.alt || virt.shift) {
                        consumed = true;
                    }
                }
            }
            egui::Event::PointerButton {
                pos,
                button,
                pressed,
                modifiers,
                ..
            } => {
                *modifiers = or_mods(*modifiers, virt);
                let on_bar = bar.rect.is_some_and(|r| r.contains(*pos));
                if *button == egui::PointerButton::Primary {
                    if *pressed && !on_bar {
                        bar.press_fired = false;
                        if bar.rc_once {
                            // "RC": this tap is a right-click.
                            bar.rc_once = false;
                            bar.rc_release = true;
                            *button = egui::PointerButton::Secondary;
                        } else if !bar.long_press_off {
                            bar.press = Some((*pos, std::time::Instant::now()));
                        }
                    } else if !*pressed {
                        bar.press = None;
                        if bar.rc_release {
                            bar.rc_release = false;
                            *button = egui::PointerButton::Secondary;
                        } else if bar.press_fired {
                            // The long press already sent its own click; drop the real release.
                            bar.press_fired = false;
                            continue;
                        }
                    }
                }
                if *pressed && !on_bar && (virt.ctrl || virt.alt || virt.shift) {
                    consumed = true;
                }
            }
            egui::Event::PointerMoved(p) => {
                if let Some((start, _)) = bar.press {
                    if (*p - start).length() > 14.0 {
                        bar.press = None;
                    }
                }
            }
            egui::Event::MouseWheel { modifiers, .. } => {
                *modifiers = or_mods(*modifiers, virt);
            }
            _ => {}
        }
        out.push(event);
    }
    if let Some((pos, since)) = bar.press {
        if since.elapsed() >= std::time::Duration::from_millis(550) {
            bar.press = None;
            bar.press_fired = true;
            let m = or_mods(physical, bar.virtual_mods());
            for (button, pressed) in [
                (egui::PointerButton::Primary, false),
                (egui::PointerButton::Secondary, true),
                (egui::PointerButton::Secondary, false),
            ] {
                out.push(egui::Event::PointerButton {
                    pos,
                    button,
                    pressed,
                    modifiers: m,
                });
            }
        }
    }
    raw.events = out;

    if consumed {
        bar.release_once();
    }
    let effective = or_mods(physical, bar.virtual_mods());
    if effective != bar.last || (had_mod_event && (virt.ctrl || virt.alt || virt.shift)) {
        raw.events.push(egui::Event::ModifiersChanged(effective));
    }
    bar.last = effective;
}


struct PhotoCraftAndroidApp {
    inner: PhotocraftApp,
    bridge: AndroidFileBridge,
    picker: Arc<PickerState>,
    ime: ime_bridge::ime::ImeBridge,
    modifiers: egui::Modifiers,
    shortcuts: ShortcutBar,
    // Files the system asked us to open are staged by the Activity; this is the shared queue the
    // app's own inbox drains. `intent_warmup` skips the first couple of frames so the app has
    // finished its own start-up, and goes negative (stop polling) if the host has no intent bridge.
    intent_inbox: Inbox,
    intent_warmup: i64,
    // Staged working copies under media/ the app still refers to (see `staged_prune_tick`), and
    // when the one-time orphan sweep of the directory is due.
    staged_seen: Vec<String>,
    staged_sweep_at: Option<std::time::Instant>,
}

/// Read the files the system asked this app to open (an `ACTION_VIEW`/`ACTION_SEND` it is registered
/// for) into the app's import inbox, exactly like a document chosen in the picker. Each staged path
/// is read once; a launcher without the intent host stops the poll after one error.
fn drain_incoming(bridge: &AndroidFileBridge, inbox: &Inbox, warmup: &mut i64) {
    if *warmup > 0 {
        *warmup -= 1;
        return;
    }
    if *warmup < 0 {
        return;
    }
    for _ in 0..16 {
        match bridge.intent_poll() {
            Ok(Some(path)) => match fs::read(&path) {
                Ok(bytes) => {
                    let name = Path::new(&path)
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "imported-file".to_string());
                    log::info!("Opening the file the system handed the app: {path}");
                    inbox.lock().unwrap_or_else(|e| e.into_inner()).push((name, bytes));
                }
                Err(error) => log::error!("Could not read the incoming file {path}: {error}"),
            },
            Ok(None) => break,
            Err(error) => {
                log::info!("Launcher does not report incoming files: {error}");
                *warmup = -1;
                break;
            }
        }
    }
}

impl PhotoCraftAndroidApp {
    /// The staged copy of a document, if `path` names one: a *direct* child of the app's own
    /// `media/` directory (see PhotoCraftActivity.newStageFile). Anything else on the device is
    /// never touched.
    fn staged_media_file(&self, path: &str) -> Option<PathBuf> {
        let files = self.bridge.files_dir().ok()?;
        let media = files.join("media");
        let file = PathBuf::from(path);
        (file.parent() == Some(media.as_path())).then_some(file)
    }

    /// Delete every staged copy under `media/` that neither the recent list nor an open document
    /// refers to, sparing files written in the last minute (a document may be mid-import). This
    /// clears out copies orphaned before this pruning existed, or by an app killed mid-session.
    fn staged_sweep_orphans(bridge: &AndroidFileBridge, recent: &[String], open: &[String]) {
        let Ok(files) = bridge.files_dir() else { return };
        let media = files.join("media");
        let Ok(entries) = fs::read_dir(&media) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            if recent.iter().any(|keep| Path::new(keep) == path.as_path())
                || open.iter().any(|keep| Path::new(keep) == path.as_path())
            {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            if !meta.is_file() {
                continue;
            }
            let fresh = meta
                .modified()
                .ok()
                .and_then(|at| at.elapsed().ok())
                .is_some_and(|age| age < std::time::Duration::from_secs(60));
            if fresh {
                continue;
            }
            let name = path.to_string_lossy().into_owned();
            match fs::remove_file(&path) {
                Ok(()) => log::info!("deleted an orphaned staged copy: {name}"),
                Err(error) => log::warn!("could not delete the staged copy {name}: {error}"),
            }
        }
    }

    /// When an entry leaves the recent-files list (the list is cleared, an entry is removed with
    /// its ×, or it ages out of the capped list) and no open document is showing that file, its
    /// staged copy under `media/` is deleted: the document's original still lives at its SAF
    /// location. The `uri:` mapping in SharedPreferences is deliberately kept, so re-opening the
    /// same document reuses the same path and a later save still reaches the original. A delayed
    /// sweep of the whole directory clears out copies this diff never saw (an older session, or a
    /// crash); it runs once, half a minute after start, to let session restore finish first.
    fn staged_prune_tick(&mut self) {
        let current: Vec<String> = self.inner.ui.recent_files.clone();
        let open: Vec<String> = self
            .inner
            .session
            .documents()
            .iter()
            .filter_map(|doc| doc.path.clone())
            .collect::<Vec<_>>();
        for stale in self
            .staged_seen
            .iter()
            .filter(|path| !current.contains(*path) && !open.contains(*path))
        {
            let Some(file) = self.staged_media_file(stale) else { continue };
            match fs::remove_file(&file) {
                Ok(()) => log::info!("deleted the staged copy of a recent file the app let go: {stale}"),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => log::warn!("could not delete the staged copy {stale}: {error}"),
            }
        }
        if self.staged_sweep_at.is_some_and(|at| std::time::Instant::now() >= at) {
            self.staged_sweep_at = None;
            Self::staged_sweep_orphans(&self.bridge, &current, &open);
        }
        self.staged_seen = current;
    }
}

impl eframe::App for PhotoCraftAndroidApp {
    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.picker.poll(&self.bridge, ctx);
        // Deliver any file the system asked us to open through the app's own inbox.
        drain_incoming(&self.bridge, &self.intent_inbox, &mut self.intent_warmup);
        self.staged_prune_tick();
        self.inner.logic(ctx, frame);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        system_fonts::install_system_fonts(ui.ctx());
        self.inner.ui(ui, _frame);
        let screen = ui.max_rect();
        self.shortcuts.ui(ui.ctx(), screen);
    }

    fn raw_input_hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        self.ime.poll(raw);
        apply_android_modifiers(&mut self.modifiers, raw);
        apply_virtual_modifiers(&mut self.shortcuts, self.modifiers, raw);
        if self.shortcuts.press.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
        if let Some(show) = self.shortcuts.kb_request.take() {
            if show {
                self.bridge.app.show_soft_input(false);
            } else {
                self.bridge.app.hide_soft_input(false);
            }
        }
        // Soft keyboard: commit letters immediately while an on-screen Ctrl/Alt is armed.
        let armed = self.shortcuts.ctrl.on() || self.shortcuts.alt.on();
        if armed != self.shortcuts.java_mode {
            self.shortcuts.java_mode = armed;
            self.bridge.set_shortcut_mode(armed);
        }
        self.inner.raw_input_hook(ctx, raw);
    }
}

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(android_app: AndroidApp) {
    log::info!("{ANDROID_SHELL_MARKER} ready; {}", ime_bridge::ime::IME_MARKER);
    egui_winit::android_activity::set_activity(android_app.activity_as_ptr());
    system_fonts::install_font_env(&android_app);
    let bridge = match AndroidFileBridge::new(android_app.clone()) {
        Ok(bridge) => bridge,
        Err(error) => {
            log::error!("PhotoCraft Android startup failed: {error}");
            return;
        }
    };
    let files_dir = match bridge.files_dir() {
        Ok(path) => path,
        Err(error) => {
            log::error!("Could not locate PhotoCraft app-private storage: {error}");
            return;
        }
    };
    let prefs_path = files_dir.join("PhotoCraft").join("preferences.json");
    if let Some(parent) = prefs_path.parent() {
        if let Err(error) = fs::create_dir_all(parent) {
            log::error!("Could not create preferences directory: {error}");
        }
    }
    let picker = Arc::new(PickerState::default());
    let intent_inbox: Inbox = Arc::new(Mutex::new(Vec::new()));
    let mut options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("PhotoCraft")
            .with_drag_and_drop(true),
        renderer: eframe::Renderer::Wgpu,
        android_app: Some(android_app),
        ..Default::default()
    };
    // The adapter's real texture and buffer limits, so large documents stay on the GPU canvas;
    // Vulkan first, GLES on devices without a usable Vulkan driver — the same setup the desktop
    // app and the generated launchers use.
    photocraft_ui_egui::gpu_canvas::use_adapter_limits(&mut options.wgpu_options.wgpu_setup);
    if let eframe::egui_wgpu::WgpuSetup::CreateNew(create) = &mut options.wgpu_options.wgpu_setup {
        create.instance_descriptor.backends =
            eframe::wgpu::Backends::VULKAN | eframe::wgpu::Backends::GL;
    }
    let app_bridge = bridge.clone_for_services();
    let app_picker = picker.clone();
    let app_intent_inbox = intent_inbox.clone();
    let result = eframe::run_native(
        "PhotoCraft",
        options,
        Box::new(move |cc| {
            PhotocraftApp::setup_context(&cc.egui_ctx, photocraft_ui_egui::theme::ThemeKind::Pro);
            let mut app = PhotocraftApp::new(
                Session::new(),
                services(
                    app_bridge.clone_for_services(),
                    app_picker.clone(),
                    prefs_path.clone(),
                    app_intent_inbox.clone(),
                ),
            );
            // Hand the canvas the live render state when the device is a real GPU, and record the
            // adapter for Help › System Info either way — without this the canvas stays on the CPU
            // compositor and System Info reports no adapter ("unknown").
            if let Some(rs) = cc.wgpu_render_state.clone() {
                let info = rs.adapter.get_info();
                log::info!("wgpu adapter: {} ({:?})", info.name, info.backend);
                app.perf.gpu_info.set_adapter(&info);
                if info.device_type != eframe::wgpu::DeviceType::Cpu {
                    app.set_wgpu(rs);
                }
            }
            Ok(Box::new(PhotoCraftAndroidApp {
                inner: app,
                bridge: app_bridge.clone_for_services(),
                picker: app_picker.clone(),
                ime: ime_bridge::ime::ImeBridge::new(&app_bridge.app),
                modifiers: egui::Modifiers::default(),
                shortcuts: ShortcutBar::default(),
                intent_inbox: app_intent_inbox.clone(),
                intent_warmup: 2,
                staged_seen: Vec::new(),
                staged_sweep_at: Some(std::time::Instant::now() + std::time::Duration::from_secs(30)),
            }))
        }),
    );
    if let Err(error) = result {
        log::error!("PhotoCraft Android exited with an error: {error}");
    }
}
