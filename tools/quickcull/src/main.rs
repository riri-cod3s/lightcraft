//! QuickCull: arrow through a folder of finished photos, tap a key to pick the keepers, then copy
//! (or compress) the picks into another folder. Decoding runs on worker threads and the next few
//! photos are prefetched, so moving with the arrow keys never waits.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod export;
mod loader;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, channel};

use eframe::egui::{self, Color32, Key, Pos2, Rect, RichText, Stroke, TextureHandle, Vec2};

use loader::{Loader, Slot};

const IMAGE_EXTS: &[&str] = &["jpg", "jpeg", "png", "webp", "tif", "tiff"];
const PREFETCH_AHEAD: usize = 6;
const PREFETCH_BEHIND: usize = 2;
const KEEP_WINDOW: usize = 10;
const DEFAULT_TARGET: usize = 60;

fn main() -> eframe::Result<()> {
    let arg = std::env::args_os().nth(1).map(PathBuf::from);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1400.0, 900.0])
            .with_title("QuickCull"),
        ..Default::default()
    };
    eframe::run_native(
        "QuickCull",
        options,
        Box::new(move |_cc| Ok(Box::new(App::new(arg)))),
    )
}

fn list_images(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let rd = std::fs::read_dir(dir).map_err(|e| format!("can't read {}: {e}", dir.display()))?;
    let mut files: Vec<PathBuf> = rd
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| IMAGE_EXTS.contains(&e.to_ascii_lowercase().as_str()))
        })
        .collect();
    files.sort_by_key(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase()));
    Ok(files)
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

struct ExportJob {
    total: usize,
    done: Arc<AtomicUsize>,
    finished: Arc<AtomicBool>,
    errors: Receiver<String>,
    error_list: Vec<String>,
}

struct App {
    dir: Option<PathBuf>,
    files: Vec<PathBuf>,
    selected: HashSet<String>,
    idx: usize,
    only_selected: bool,
    target: usize,
    loader: Loader,
    textures: std::collections::HashMap<usize, TextureHandle>,
    last_tex: Option<TextureHandle>,
    message: Option<String>,
    show_export: bool,
    export_dir: String,
    compress: bool,
    quality: u8,
    max_edge: u32,
    job: Option<ExportJob>,
}

impl App {
    fn new(arg: Option<PathBuf>) -> Self {
        let mut app = Self {
            dir: None,
            files: Vec::new(),
            selected: HashSet::new(),
            idx: 0,
            only_selected: false,
            target: DEFAULT_TARGET,
            loader: Loader::new(),
            textures: Default::default(),
            last_tex: None,
            message: None,
            show_export: false,
            export_dir: String::new(),
            compress: false,
            quality: 85,
            max_edge: 2560,
            job: None,
        };
        if let Some(dir) = arg {
            app.open_dir(dir);
        }
        app
    }

    fn open_dir(&mut self, dir: PathBuf) {
        match list_images(&dir) {
            Ok(files) => {
                self.selected = load_selection(&dir);
                // Forget picks whose file is gone.
                let names: HashSet<String> = files.iter().map(|p| file_name(p)).collect();
                self.selected.retain(|n| names.contains(n));
                self.export_dir = dir.join("selected").to_string_lossy().into_owned();
                self.message = if files.is_empty() {
                    Some("No images (jpg, png, webp, tiff) in that folder.".into())
                } else {
                    None
                };
                self.files = files;
                self.dir = Some(dir);
                self.idx = 0;
                self.only_selected = false;
                self.textures.clear();
                self.last_tex = None;
                self.loader.reset();
            }
            Err(e) => self.message = Some(e),
        }
    }

    fn is_selected(&self, i: usize) -> bool {
        self.files.get(i).is_some_and(|p| self.selected.contains(&file_name(p)))
    }

    fn save(&mut self) {
        if let Some(dir) = &self.dir
            && let Err(e) = save_selection(dir, &self.selected)
        {
            self.message = Some(format!("couldn't save picks: {e}"));
        }
    }

    fn toggle(&mut self, i: usize, to: Option<bool>) {
        let Some(p) = self.files.get(i) else { return };
        let name = file_name(p);
        let want = to.unwrap_or(!self.selected.contains(&name));
        if want {
            self.selected.insert(name);
        } else {
            self.selected.remove(&name);
        }
        self.save();
    }

    /// Next index in `dir` (+1/-1) honouring the "only picked" filter.
    fn step(&self, from: usize, dir: isize) -> Option<usize> {
        let mut i = from as isize + dir;
        while i >= 0 && (i as usize) < self.files.len() {
            if !self.only_selected || self.is_selected(i as usize) {
                return Some(i as usize);
            }
            i += dir;
        }
        None
    }

    fn go(&mut self, dir: isize, count: usize) {
        for _ in 0..count {
            match self.step(self.idx, dir) {
                Some(i) => self.idx = i,
                None => break,
            }
        }
    }

    fn jump_to_edge(&mut self, last: bool) {
        let (start, dir) = if last { (self.files.len() as isize, -1) } else { (-1, 1) };
        let mut i = start + dir;
        while i >= 0 && (i as usize) < self.files.len() {
            if !self.only_selected || self.is_selected(i as usize) {
                self.idx = i as usize;
                return;
            }
            i += dir;
        }
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        let typing = ctx.egui_wants_keyboard_input();
        if self.files.is_empty() || typing {
            return;
        }
        let (mut go, mut toggle, mut pick_next, mut edge, mut filter, mut export) =
            (0isize, false, false, None, false, false);
        ctx.input(|i| {
            if i.key_pressed(Key::ArrowRight) || i.key_pressed(Key::D) {
                go += 1;
            }
            if i.key_pressed(Key::ArrowLeft) || i.key_pressed(Key::A) {
                go -= 1;
            }
            if i.key_pressed(Key::PageDown) {
                go += 10;
            }
            if i.key_pressed(Key::PageUp) {
                go -= 10;
            }
            toggle = i.key_pressed(Key::Space) || i.key_pressed(Key::ArrowUp);
            pick_next = i.key_pressed(Key::Enter) || i.key_pressed(Key::S);
            if i.key_pressed(Key::Home) {
                edge = Some(false);
            }
            if i.key_pressed(Key::End) {
                edge = Some(true);
            }
            filter = i.key_pressed(Key::Tab);
            export = i.key_pressed(Key::E);
        });
        if let Some(last) = edge {
            self.jump_to_edge(last);
        }
        if toggle {
            self.toggle(self.idx, None);
        }
        if ctx.input(|i| i.key_pressed(Key::ArrowDown) || i.key_pressed(Key::X)) {
            self.toggle(self.idx, Some(false));
        }
        if pick_next {
            self.toggle(self.idx, Some(true));
            self.go(1, 1);
        }
        if go != 0 {
            self.go(go.signum(), go.unsigned_abs());
        }
        if filter {
            self.only_selected = !self.only_selected;
            if self.only_selected && !self.is_selected(self.idx) {
                // Land on the nearest pick.
                if let Some(i) = self.step(self.idx, 1).or_else(|| self.step(self.idx, -1)) {
                    self.idx = i;
                }
            }
        }
        if export {
            self.show_export = !self.show_export;
        }
    }

    /// Ask the loader for the current photo first, then its neighbours; drop textures far away.
    fn prefetch(&mut self, ctx: &egui::Context) {
        if self.files.is_empty() {
            return;
        }
        self.loader.set_current(self.idx);
        let mut want = vec![self.idx];
        let (mut a, mut b) = (self.idx, self.idx);
        for n in 0..PREFETCH_AHEAD.max(PREFETCH_BEHIND) {
            if n < PREFETCH_AHEAD
                && let Some(i) = self.step(a, 1)
            {
                want.push(i);
                a = i;
            }
            if n < PREFETCH_BEHIND
                && let Some(i) = self.step(b, -1)
            {
                want.push(i);
                b = i;
            }
        }
        for i in want {
            if let Some(p) = self.files.get(i) {
                self.loader.request(i, p.clone());
            }
        }
        for (i, slot) in self.loader.poll() {
            if let Slot::Ready(img) = slot {
                let tex = ctx.load_texture(format!("img{i}"), img, egui::TextureOptions::LINEAR);
                self.textures.insert(i, tex);
            }
        }
        let cur = self.idx;
        let far = |i: usize| i.abs_diff(cur) > KEEP_WINDOW;
        self.textures.retain(|&i, _| !far(i));
        self.loader.forget_if(far);
    }

    fn start_export(&mut self) {
        let Some(dir) = self.dir.clone() else { return };
        let dest = PathBuf::from(self.export_dir.trim());
        let mut picked: Vec<PathBuf> = self
            .files
            .iter()
            .filter(|p| self.selected.contains(&file_name(p)))
            .cloned()
            .collect();
        picked.sort();
        if picked.is_empty() {
            self.message = Some("Nothing picked yet.".into());
            return;
        }
        if export::same_dir(&dir, &dest) {
            self.message = Some("Choose a different folder than the source.".into());
            return;
        }
        let (tx, rx) = channel();
        let done = Arc::new(AtomicUsize::new(0));
        let finished = Arc::new(AtomicBool::new(false));
        let settings = export::Settings {
            compress: self.compress,
            quality: self.quality,
            max_edge: self.max_edge,
        };
        self.job = Some(ExportJob {
            total: picked.len(),
            done: done.clone(),
            finished: finished.clone(),
            errors: rx,
            error_list: Vec::new(),
        });
        std::thread::spawn(move || {
            export::run(picked, dest, settings, done, tx);
            finished.store(true, Ordering::SeqCst);
        });
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("top").show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                if ui.button("📂 Open folder").clicked() {
                    if let Some(d) = rfd::FileDialog::new().pick_folder() {
                        self.open_dir(d);
                    }
                }
                ui.separator();
                let n = self.selected.len();
                let color = if n == 0 {
                    Color32::GRAY
                } else if n <= self.target {
                    Color32::from_rgb(90, 200, 120)
                } else {
                    Color32::from_rgb(240, 170, 60)
                };
                ui.label(RichText::new(format!("✔ {n} / {}", self.target)).strong().color(color));
                ui.add(egui::DragValue::new(&mut self.target).range(1..=5000).prefix("target "));
                ui.separator();
                if !self.files.is_empty() {
                    ui.label(format!("photo {} of {}", self.idx + 1, self.files.len()));
                }
                ui.toggle_value(&mut self.only_selected, "Show picks only (Tab)");
                ui.separator();
                if ui.button("⬇ Copy picks… (E)").clicked() {
                    self.show_export = !self.show_export;
                }
            });
        });
    }

    fn export_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_export;
        egui::Window::new("Copy picks to a folder").open(&mut open).resizable(false).show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("To:");
                ui.text_edit_singleline(&mut self.export_dir);
                if ui.button("Browse…").clicked() {
                    if let Some(d) = rfd::FileDialog::new().pick_folder() {
                        self.export_dir = d.to_string_lossy().into_owned();
                    }
                }
            });
            ui.checkbox(&mut self.compress, "Compress (re-encode as JPEG)");
            ui.add_enabled_ui(self.compress, |ui| {
                ui.add(egui::Slider::new(&mut self.quality, 40..=100).text("JPEG quality"));
                ui.horizontal(|ui| {
                    ui.label("Longest side (px, 0 = keep size):");
                    ui.add(egui::DragValue::new(&mut self.max_edge).range(0..=20000).speed(50));
                });
            });
            if !self.compress {
                ui.weak("Files are copied byte for byte (original quality, metadata kept).");
            } else {
                ui.weak("Compressed copies are saved as .jpg; EXIF metadata is not kept.");
            }
            ui.add_space(4.0);
            let busy = self.job.as_ref().is_some_and(|j| !j.finished.load(Ordering::SeqCst));
            if ui
                .add_enabled(!busy, egui::Button::new(format!("Copy {} picked photos", self.selected.len())))
                .clicked()
            {
                self.start_export();
            }
            if let Some(job) = &mut self.job {
                while let Ok(e) = job.errors.try_recv() {
                    job.error_list.push(e);
                }
                let done = job.done.load(Ordering::SeqCst);
                ui.add(egui::ProgressBar::new(done as f32 / job.total.max(1) as f32).text(format!("{done}/{}", job.total)));
                if job.finished.load(Ordering::SeqCst) {
                    if job.error_list.is_empty() {
                        ui.colored_label(Color32::from_rgb(90, 200, 120), "Done ✔");
                    } else {
                        ui.colored_label(Color32::from_rgb(240, 120, 90), format!("{} failed:", job.error_list.len()));
                        for e in &job.error_list {
                            ui.label(e);
                        }
                    }
                }
            }
        });
        self.show_export = open;
    }

    fn viewer(&mut self, ui: &mut egui::Ui) {
        let avail = ui.available_rect_before_wrap();
        let Some(path) = self.files.get(self.idx) else {
            ui.centered_and_justified(|ui| {
                ui.heading(self.message.clone().unwrap_or_else(|| "Open a folder to start (📂 above).".into()));
            });
            return;
        };
        let name = file_name(path);
        let tex = self.textures.get(&self.idx).cloned();
        let shown = tex.clone().or_else(|| self.last_tex.clone());
        if let Some(t) = &tex {
            self.last_tex = Some(t.clone());
        }
        let selected = self.is_selected(self.idx);
        if let Some(t) = shown {
            let sz = t.size_vec2();
            let scale = (avail.width() / sz.x).min(avail.height() / sz.y);
            let rect = Rect::from_center_size(avail.center(), sz * scale);
            let tint = if tex.is_some() { Color32::WHITE } else { Color32::from_gray(90) };
            ui.painter().image(t.id(), rect, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), tint);
            if selected {
                ui.painter().rect_stroke(rect, 0.0, Stroke::new(6.0, Color32::from_rgb(70, 200, 110)), egui::StrokeKind::Outside);
            }
        }
        if tex.is_none() {
            let msg = match self.loader.failure(self.idx) {
                Some(e) => format!("Can't open {name}: {e}"),
                None => "loading…".into(),
            };
            ui.painter().text(avail.center(), egui::Align2::CENTER_CENTER, msg, egui::FontId::proportional(20.0), Color32::LIGHT_GRAY);
        }
        let label = if selected { format!("✔ PICKED   {name}") } else { name };
        ui.painter().text(
            avail.left_top() + Vec2::new(10.0, 10.0),
            egui::Align2::LEFT_TOP,
            label,
            egui::FontId::proportional(18.0),
            if selected { Color32::from_rgb(110, 230, 150) } else { Color32::WHITE },
        );
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.handle_keys(&ctx);
        self.prefetch(&ctx);
        self.top_bar(ui);
        egui::Panel::bottom("help").show(ui, |ui| {
            ui.weak("←/→ move   Space/↑ pick   ↓/X unpick   Enter/S pick + next   Tab picks only   PgUp/PgDn ±10   Home/End   E copy picks");
        });
        self.export_window(&ctx);
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(Color32::from_gray(18)))
            .show(ui, |ui| self.viewer(ui));
        if self.loader.busy() || self.job.as_ref().is_some_and(|j| !j.finished.load(Ordering::SeqCst)) {
            ctx.request_repaint_after(std::time::Duration::from_millis(16));
        }
    }
}

fn selection_path(dir: &Path) -> PathBuf {
    dir.join(".quickcull.json")
}

fn load_selection(dir: &Path) -> HashSet<String> {
    std::fs::read(selection_path(dir))
        .ok()
        .and_then(|b| serde_json::from_slice::<Vec<String>>(&b).ok())
        .map(|v| v.into_iter().collect())
        .unwrap_or_default()
}

fn save_selection(dir: &Path, sel: &HashSet<String>) -> Result<(), String> {
    let mut names: Vec<&String> = sel.iter().collect();
    names.sort();
    let json = serde_json::to_vec_pretty(&names).map_err(|e| e.to_string())?;
    let path = selection_path(dir);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_with(n: usize, picked: &[usize]) -> App {
        let mut a = App::new(None);
        a.files = (0..n).map(|i| PathBuf::from(format!("{i:03}.jpg"))).collect();
        for &i in picked {
            a.selected.insert(format!("{i:03}.jpg"));
        }
        a
    }

    #[test]
    fn navigation_respects_picks_only_filter() {
        let mut a = app_with(10, &[2, 5, 9]);
        a.go(1, 3);
        assert_eq!(a.idx, 3);
        a.only_selected = true;
        a.go(1, 1);
        assert_eq!(a.idx, 5);
        a.go(1, 5);
        assert_eq!(a.idx, 9, "stops at the last pick");
        a.go(-1, 1);
        assert_eq!(a.idx, 5);
    }

    #[test]
    fn selection_round_trips_through_the_sidecar_file() {
        let dir = std::env::temp_dir().join(format!("quickcull-sel-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sel: HashSet<String> = ["b.jpg", "a.jpg"].iter().map(|s| s.to_string()).collect();
        save_selection(&dir, &sel).unwrap();
        assert_eq!(load_selection(&dir), sel);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
