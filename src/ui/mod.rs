mod compare;
mod strata;
mod theme;
pub mod treemap;

use crate::category::{self, Category};
use crate::delete::{self, Method, Plan};
use crate::filter::{self, FilterSpec, Kind};
use crate::model::{NONE, ScanMethod, Tree, flags};
use crate::platform::{self, DriveInfo};
use crate::scan::{self, ScanMsg, ScanRequest, mft::Progress};
use crate::util::{fmt_count, fmt_date, fmt_size};
use egui::{Align, Align2, Color32, FontId, Key, Pos2, RichText, Sense, Stroke, StrokeKind, Ui, pos2, vec2};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use theme::{ACCENT, DANGER, OK, WARN};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Analyze,
    Compare,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Strata,
    Treemap,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum ColorMode {
    Folder,
    Type,
    Age,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Overview,
    Cleanup,
    Search,
    Largest,
    Dupes,
    Changes,
}

enum Action {
    Select(u32),
    /// Selection from the tree list: the chart follows (zooms to the folder).
    TreeSelect(u32),
    Zoom(u32),
    ZoomOut,
    Toggle(u32),
    Delete(Vec<u32>, Method),
    Reveal(u32),
    Open(u32),
    CopyPath(u32),
    Scope(u32),
    Check(u32, bool),
    CheckMany(Vec<u32>, bool),
    HighlightExt(Option<u16>),
}

struct TmCache {
    key: (u32, usize, usize, bool, ColorMode, u64, Option<u16>, Option<Category>),
    layout: treemap::Layout,
    tex: egui::TextureHandle,
}

struct Confirm {
    plan: Plan,
    method: Method,
    typed: String,
    ack: bool,
}

struct Report {
    ok: usize,
    freed: u64,
    failures: Vec<(String, String)>,
    method: Method,
}

/// Everything except the tree itself, so the tree can be borrowed alongside.
struct State {
    drives: Vec<DriveInfo>,
    drive_pick: usize,
    elevated: bool,
    notice: Option<(String, Color32)>,

    ext_colors: Vec<Color32>,
    view_root: u32,
    selected: Option<u32>,
    hovered: Option<u32>,
    /// Item whose right-click menu is open on a chart (kept across frames, since the
    /// pointer leaves the item as soon as it moves onto the menu).
    menu_node: Option<u32>,
    expanded: HashSet<u32>,
    rows: Vec<(u32, u16)>,
    rows_dirty: bool,
    scroll_to_selected: bool,

    view: View,
    color_mode: ColorMode,
    use_alloc: bool,
    highlight_ext: Option<u16>,
    generation: u64,
    tm: Option<TmCache>,
    strata: strata::StrataState,
    show_free: bool,
    cats: Vec<Category>,
    highlight_cat: Option<Category>,
    cat_cache: Option<((u32, u64), [(u64, u64, u64); 10])>,

    tab: Tab,
    search_text: String,
    search_results: Vec<u32>,
    search_err: Option<String>,
    search_due: Option<Instant>,
    focus_search: bool,
    filter: FilterSpec,
    filter_scope: Option<u32>,
    filter_results: Vec<u32>,
    filter_err: Option<String>,
    filter_ran: bool,
    largest: Option<((u32, u64, bool), Vec<u32>)>,
    checked: HashSet<u32>,

    confirm: Option<Confirm>,
    job: Option<(Arc<delete::Job>, Vec<delete::Item>, Method)>,
    report: Option<Report>,
    now: i64,
    perf_overlay: bool,
    dupes: Option<Vec<crate::dupes::Group>>,
    dupes_job: Option<(Arc<crate::dupes::Progress>, crossbeam_channel::Receiver<Vec<crate::dupes::Group>>, Instant)>,
    dupes_min: String,
    history: Option<(String, Vec<(PathBuf, crate::snapshot::Meta)>)>,
    changes: Option<(crate::snapshot::Meta, Vec<crate::snapshot::Change>)>,
    changes_old: Option<Box<Tree>>,
    changes_min: String,
    changes_loading: Option<crossbeam_channel::Receiver<Result<(Tree, crate::snapshot::Meta), String>>>,
    /// Viewing a saved scan (read-only).
    viewing_saved: Option<crate::snapshot::Meta>,
    /// Remote Desktop mode: no hover dimming/animations/floating tooltips on charts.
    remote: bool,
    perf_hist: Vec<f32>,
    perf_ui_ms: f32,
    ext_order: Option<((u64, bool), Vec<usize>)>,
}

pub struct App {
    tree: Option<Tree>,
    st: State,
    scan_rx: Option<crossbeam_channel::Receiver<ScanMsg>>,
    progress: Arc<Progress>,
    scan_started: Instant,
    scanning: Option<PathBuf>,
    logo: egui::TextureHandle,
    mode: Mode,
    compare: compare::CompareState,
    ssh_form: Option<SshForm>,
    scan_is_remote: bool,
    pending_remote_rescan: Option<(Arc<crate::remote::Conn>, PathBuf)>,
    trust_prompt: Option<(crate::remote::Target, String, PathBuf)>,
    last_ssh: Option<SshForm>,
}

#[derive(Clone)]
struct SshForm {
    host: String,
    port: String,
    user: String,
    use_key: bool,
    password: String,
    key_path: String,
    key_pass: String,
    path: String,
    error: Option<String>,
}

impl Default for SshForm {
    fn default() -> Self {
        let home = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")).unwrap_or_default();
        SshForm {
            host: String::new(),
            port: "22".into(),
            user: "root".into(),
            use_key: false,
            password: String::new(),
            key_path: if home.is_empty() { String::new() } else { format!("{home}{}.ssh{}id_ed25519", std::path::MAIN_SEPARATOR, std::path::MAIN_SEPARATOR) },
            key_pass: String::new(),
            path: "/".into(),
            error: None,
        }
    }
}

impl SshForm {
    fn target(&self) -> Result<crate::remote::Target, String> {
        if self.host.trim().is_empty() || self.user.trim().is_empty() {
            return Err("host and user are required".into());
        }
        Ok(crate::remote::Target {
            host: self.host.trim().into(),
            port: self.port.trim().parse().map_err(|_| "bad port".to_string())?,
            user: self.user.trim().into(),
            password: (!self.use_key && !self.password.is_empty()).then(|| self.password.clone()),
            key_path: (self.use_key && !self.key_path.trim().is_empty()).then(|| PathBuf::from(self.key_path.trim())),
            key_pass: (self.use_key && !self.key_pass.is_empty()).then(|| self.key_pass.clone()),
            trust_fingerprint: None,
        })
    }
}

impl App {
    pub fn new(ctx: &egui::Context, initial: Option<PathBuf>) -> Self {
        theme::apply(ctx);
        let drives = platform::list_drives();
        let mut app = App {
            tree: None,
            st: State {
                drive_pick: drives.iter().position(|d| d.root.to_string_lossy().starts_with('C')).unwrap_or(0),
                drives,
                elevated: platform::is_elevated(),
                notice: None,
                ext_colors: Vec::new(),
                view_root: 0,
                selected: None,
                hovered: None,
                menu_node: None,
                expanded: HashSet::new(),
                rows: Vec::new(),
                rows_dirty: true,
                scroll_to_selected: false,
                view: View::Strata,
                color_mode: ColorMode::Folder,
                use_alloc: true,
                highlight_ext: None,
                generation: 0,
                tm: None,
                strata: strata::StrataState::default(),
                show_free: true,
                cats: Vec::new(),
                highlight_cat: None,
                cat_cache: None,
                tab: Tab::Overview,
                search_text: String::new(),
                search_results: Vec::new(),
                search_err: None,
                search_due: None,
                focus_search: false,
                filter: FilterSpec::default(),
                filter_scope: None,
                filter_results: Vec::new(),
                filter_err: None,
                filter_ran: false,
                largest: None,
                checked: HashSet::new(),
                confirm: None,
                job: None,
                report: None,
                now: chrono::Local::now().timestamp(),
                perf_overlay: false,
                dupes: None,
                dupes_job: None,
                dupes_min: "1 MB".into(),
                history: None,
                changes: None,
                changes_old: None,
                changes_min: "10 MB".into(),
                changes_loading: None,
                viewing_saved: None,
                remote: platform::is_remote_session(),
                perf_hist: Vec::new(),
                perf_ui_ms: 0.0,
                ext_order: None,
            },
            scan_rx: None,
            progress: Arc::new(Progress::default()),
            scan_started: Instant::now(),
            scanning: None,
            mode: Mode::Analyze,
            compare: compare::CompareState::default(),
            ssh_form: None,
            scan_is_remote: false,
            pending_remote_rescan: None,
            trust_prompt: None,
            last_ssh: None,
            logo: ctx.load_texture("logo", egui::ColorImage::from_rgba_unmultiplied([64, 64], crate::ICON_RGBA), egui::TextureOptions::LINEAR),
        };
        // `--compare <left> <right>` opens straight into a folder comparison.
        let args: Vec<String> = std::env::args().collect();
        if let Some(i) = args.iter().position(|a| a == "--compare")
            && args.len() > i + 2
        {
            app.mode = Mode::Compare;
            app.compare.preset(&args[i + 1], &args[i + 2], ctx);
        } else if let Some(p) = initial {
            app.start_scan(p, ctx);
        }
        app
    }

    fn start_scan(&mut self, path: PathBuf, ctx: &egui::Context) {
        self.start_request(ScanRequest { path, prefer_mft: true, remote: None, conn: None }, ctx);
    }

    fn open_saved(&mut self, path: PathBuf, ctx: &egui::Context) {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let c = ctx.clone();
        let p2 = path.clone();
        std::thread::spawn(move || {
            let msg = match crate::snapshot::load(&p2) {
                Ok((t, meta)) => ScanMsg::Done(Box::new(t), Some(format!("📼 Viewing saved scan of {} from {} (read-only)", meta.label, fmt_date(meta.taken)))),
                Err(e) => ScanMsg::Failed(format!("Can't open {}: {e}", p2.display())),
            };
            let _ = tx.send(msg);
            c.request_repaint();
        });
        self.progress = Arc::new(Progress::default());
        self.scan_started = Instant::now();
        self.scanning = Some(path.clone());
        self.scan_is_remote = false;
        self.st.viewing_saved = crate::snapshot::read_meta(&path).ok();
        self.scan_rx = Some(rx);
    }

    fn start_request(&mut self, req: ScanRequest, ctx: &egui::Context) {
        self.st.viewing_saved = None;
        self.progress = Arc::new(Progress::default());
        self.scan_started = Instant::now();
        let label = match (&req.remote, &req.conn) {
            (Some(t), _) => PathBuf::from(format!("{}:{}", t.label(), req.path.display())),
            (None, Some(c)) => PathBuf::from(format!("{}:{}", c.target.label(), req.path.display())),
            _ => req.path.clone(),
        };
        self.scanning = Some(label);
        self.scan_is_remote = req.remote.is_some() || req.conn.is_some();
        self.st.notice = None;
        self.scan_rx = Some(scan::start(req, self.progress.clone(), ctx.clone()));
    }

    fn poll_scan(&mut self) {
        let Some(rx) = &self.scan_rx else { return };
        let Ok(msg) = rx.try_recv() else { return };
        self.scan_rx = None;
        self.scanning = None;
        match msg {
            ScanMsg::Done(t, note) => {
                if t.remote.is_none() {
                    self.last_ssh = None;
                }
                let st = &mut self.st;
                st.ext_colors = theme::ext_colors(&t);
                st.view_root = t.root;
                st.selected = None;
                st.expanded.clear();
                st.expanded.insert(t.root);
                st.rows_dirty = true;
                st.tm = None;
                st.strata = strata::StrataState::default();
                st.cats = category::ext_categories(&t);
                st.dupes = None;
                st.dupes_job = None;
                st.history = None;
                st.changes = None;
                st.changes_old = None;
                if st.viewing_saved.is_none() && !t.name(t.root).starts_with("synthetic") {
                    // automatic history for "what grew" comparisons (last 5 per location)
                    crate::snapshot::autosave(crate::snapshot::encode(&t), crate::snapshot::label_of(&t), 5);
                }
                st.cat_cache = None;
                st.highlight_cat = None;
                st.largest = None;
                st.checked.clear();
                st.filter_results.clear();
                st.filter_ran = false;
                st.search_results.clear();
                st.filter_scope = None;
                st.highlight_ext = None;
                st.generation += 1;
                st.now = chrono::Local::now().timestamp();
                st.notice = note.map(|n| (n, WARN));
                if !st.search_text.is_empty() {
                    st.search_due = Some(Instant::now());
                }
                self.tree = Some(*t);
            }
            ScanMsg::Failed(e) => {
                if self.scan_is_remote
                    && let Some(f) = self.last_ssh.as_mut()
                {
                    // reopen the SSH form with the error
                    f.error = Some(e.clone());
                    self.ssh_form = Some(f.clone());
                }
                self.st.notice = Some((e, DANGER));
            }
            ScanMsg::NeedTrust(target, fp, path) => self.trust_prompt = Some((target, fp, path)),
        }
    }

    fn poll_job(&mut self) {
        let Some((job, items, method)) = &self.st.job else { return };
        if !job.finished.load(Ordering::SeqCst) {
            return;
        }
        let (job, items, method) = (job.clone(), items.clone(), *method);
        self.st.job = None;
        let Some(t) = self.tree.as_mut() else { return };
        let results = std::mem::take(&mut *job.results.lock());
        let mut ok = 0;
        let mut freed = 0;
        let mut failures = Vec::new();
        for (node, r) in results {
            match r {
                Ok(()) => {
                    ok += 1;
                    freed += t.node(node).alloc;
                    t.mark_deleted(node);
                }
                Err(e) => {
                    let p = items.iter().find(|i| i.node == node).map(|i| i.path.display().to_string()).unwrap_or_default();
                    failures.push((p, e));
                }
            }
        }
        let st = &mut self.st;
        st.generation += 1;
        st.checked.retain(|&i| !t.node(i).is_deleted());
        st.filter_results.retain(|&i| !t.node(i).is_deleted());
        st.search_results.retain(|&i| !t.node(i).is_deleted());
        st.largest = None;
        st.rows_dirty = true;
        if st.selected.is_some_and(|s| t.node(s).is_deleted()) {
            st.selected = None;
        }
        while t.node(st.view_root).is_deleted() {
            st.view_root = t.node(st.view_root).parent;
        }
        if let Some(s) = st.filter_scope
            && t.node(s).is_deleted()
        {
            st.filter_scope = None;
        }
        st.report = Some(Report { ok, freed, failures, method });
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, frame: &mut eframe::Frame) {
        self.frame(ui, frame.info().cpu_usage);
    }
}

impl App {
    /// One UI frame. Called by eframe (GPU) or by the software renderer.
    pub fn frame(&mut self, ui: &mut Ui, cpu_usage: Option<f32>) {
        let frame_start = Instant::now();
        let _log = FrameLog(frame_start, cpu_usage);
        let ctx = ui.ctx().clone();
        if ctx.input(|i| i.key_pressed(Key::F12)) {
            self.st.perf_overlay = !self.st.perf_overlay;
        }
        self.st.perf_ui_ms = f32::from_bits(LAST_UI_MS.load(Ordering::Relaxed));
        if self.st.perf_overlay {
            let full = cpu_usage.unwrap_or(0.0) * 1000.0;
            self.st.perf_hist.push(full);
            if self.st.perf_hist.len() > 120 {
                self.st.perf_hist.remove(0);
            }
            let avg = self.st.perf_hist.iter().sum::<f32>() / self.st.perf_hist.len() as f32;
            let max = self.st.perf_hist.iter().cloned().fold(0.0, f32::max);
            egui::Area::new(egui::Id::unique("perf")).anchor(Align2::RIGHT_BOTTOM, vec2(-10.0, -34.0)).order(egui::Order::Foreground).show(&ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.label(RichText::new(format!("frame CPU  last {full:.1} ms · avg {avg:.1} · max {max:.1}\nui code {:.1} ms (prev frame)", self.st.perf_ui_ms)).monospace().size(11.0));
                });
            });
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        self.poll_scan();
        self.poll_job();
        if self.scan_rx.is_some() || self.st.job.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }

        let mut acts: Vec<Action> = Vec::new();
        let mut scan_req: Option<PathBuf> = None;

        // ---------- top bar ----------
        egui::Panel::top("top").frame(egui::Frame::new().fill(Color32::from_rgb(20, 22, 27)).inner_margin(egui::Margin::symmetric(12, 8))).show(ui, |ui| {
            self.toolbar(ui, &mut scan_req);
        });

        // ---------- status bar ----------
        egui::Panel::bottom("status").frame(egui::Frame::new().fill(Color32::from_rgb(20, 22, 27)).inner_margin(egui::Margin::symmetric(12, 4))).show(ui, |ui| {
            self.status_bar(ui);
        });

        if self.mode == Mode::Compare {
            egui::CentralPanel::default().show(ui, |ui| compare::ui(ui, &mut self.compare));
        } else if let Some(path) = self.scanning.clone() {
            egui::CentralPanel::default().show(ui, |ui| self.scanning_screen(ui, &path));
        } else if let Some(t) = &self.tree {
            let st = &mut self.st;
            // keyboard
            handle_keys(&ctx, t, st, &mut acts);
            run_search_if_due(t, st, &ctx);

            egui::Panel::right("side").default_size(400.0).size_range(320.0..=900.0).show(ui, |ui| {
                side_panel(ui, t, st, &mut acts);
            });
            egui::Panel::left("tree").default_size(390.0).size_range(260.0..=1100.0).show(ui, |ui| {
                tree_table(ui, t, st, &mut acts);
            });
            egui::CentralPanel::default().frame(egui::Frame::new().fill(Color32::from_rgb(18, 20, 24)).inner_margin(8)).show(ui, |ui| {
                center_view(ui, t, st, &mut acts);
            });
        } else {
            egui::CentralPanel::default().show(ui, |ui| self.welcome(ui, &mut scan_req));
        }

        self.apply_actions(acts, &ctx);
        self.modals(&ctx);

        self.ssh_modals(&ctx);
        if let Some((conn, path)) = self.pending_remote_rescan.take() {
            self.start_request(ScanRequest { path, prefer_mft: false, remote: None, conn: Some(conn) }, &ctx);
        }
        if let Some(p) = scan_req {
            self.start_scan(p, &ctx);
        }
    }
}

/// With SIZEMATTERS_FRAME_LOG set, prints how long each UI frame took (for profiling).
struct FrameLog(Instant, Option<f32>);
static LAST_UI_MS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
impl Drop for FrameLog {
    fn drop(&mut self) {
        let ms = self.0.elapsed().as_secs_f64() * 1000.0;
        LAST_UI_MS.store((ms as f32).to_bits(), Ordering::Relaxed);
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *ON.get_or_init(|| std::env::var_os("SIZEMATTERS_FRAME_LOG").is_some()) {
            eprintln!("frame {ms:.2} ms full_prev {:.2} ms", self.1.unwrap_or(0.0) * 1000.0);
        }
    }
}

// ======================================================================
// Toolbar, status, welcome, scanning
// ======================================================================

impl App {
    fn toolbar(&mut self, ui: &mut Ui, scan_req: &mut Option<PathBuf>) {
        ui.horizontal(|ui| {
            ui.add(egui::Image::new((self.logo.id(), vec2(24.0, 24.0))));
            ui.label(RichText::new("SizeMatters").size(18.0).strong().color(Color32::WHITE));
            ui.add_space(10.0);
            ui.selectable_value(&mut self.mode, Mode::Analyze, RichText::new("📊 Analyze").strong());
            ui.selectable_value(&mut self.mode, Mode::Compare, RichText::new("⇄ Compare folders").strong());
            ui.separator();
            if self.mode == Mode::Compare {
                return;
            }
            let scanning = self.scan_rx.is_some();
            let drives = &self.st.drives;
            let cur = drives.get(self.st.drive_pick).map(drive_label).unwrap_or_else(|| "Select drive".into());
            egui::ComboBox::from_id_salt("drive").selected_text(cur).width(210.0).show_ui(ui, |ui| {
                for (k, d) in drives.iter().enumerate() {
                    ui.selectable_value(&mut self.st.drive_pick, k, drive_label(d));
                }
            });
            if ui.add_enabled(!scanning, egui::Button::new(RichText::new("▶ Scan").strong()).fill(Color32::from_rgb(40, 90, 170))).on_hover_text("Scan the selected drive (F5)").clicked()
                && let Some(d) = self.st.drives.get(self.st.drive_pick)
            {
                *scan_req = Some(d.root.clone());
            }
            if ui.add_enabled(!scanning, egui::Button::new("📂 Folder…")).on_hover_text("Scan a specific folder").clicked()
                && let Some(p) = rfd::FileDialog::new().pick_folder()
            {
                *scan_req = Some(p);
            }
            if ui.add_enabled(!scanning, egui::Button::new("🌐 SSH…")).on_hover_text("Scan a remote Linux server over SSH").clicked() {
                self.ssh_form = Some(self.last_ssh.clone().unwrap_or_default());
            }
            ui.menu_button("💾", |ui| {
                if ui.add_enabled(self.tree.is_some(), egui::Button::new("Save scan as…")).clicked() {
                    if let Some(t) = &self.tree
                        && let Some(p) = rfd::FileDialog::new().add_filter("SizeMatters scan", &["smscan"]).set_file_name("scan.smscan").save_file()
                    {
                        let bytes = crate::snapshot::encode(t);
                        self.st.notice = Some(match crate::snapshot::write_compressed(&bytes, &p) {
                            Ok(()) => (format!("Saved to {}", p.display()), OK),
                            Err(e) => (format!("Save failed: {e}"), DANGER),
                        });
                    }
                    ui.close();
                }
                if ui.button("Open saved scan…").clicked() {
                    if let Some(p) = rfd::FileDialog::new().add_filter("SizeMatters scan", &["smscan"]).pick_file() {
                        self.open_saved(p, ui.ctx());
                    }
                    ui.close();
                }
                ui.separator();
                ui.label(RichText::new(format!("Scan history: {}", crate::snapshot::data_dir().join("scans").display())).weak().size(11.0));
            });
            if let Some(t) = &self.tree
                && ui.add_enabled(!scanning, egui::Button::new("⟳ Rescan")).clicked()
            {
                if let Some(conn) = &t.remote {
                    self.pending_remote_rescan = Some((conn.clone(), PathBuf::from(t.name(t.root))));
                } else {
                    *scan_req = Some(PathBuf::from(t.name(t.root)));
                }
            }
            if self.tree.is_some() {
                ui.separator();
                let st = &mut self.st;
                ui.selectable_value(&mut st.view, View::Strata, "▤ Strata").on_hover_text("The disk as stacked bars: each row splits the folder above it");
                ui.selectable_value(&mut st.view, View::Treemap, "▦ Treemap").on_hover_text("Dense cushion treemap");
                ui.separator();
                ui.selectable_value(&mut st.color_mode, ColorMode::Folder, "By folder").on_hover_text("Each top-level branch gets its own colour");
                ui.selectable_value(&mut st.color_mode, ColorMode::Type, "By type").on_hover_text("Colour files by kind: videos, images, apps…");
                ui.selectable_value(&mut st.color_mode, ColorMode::Age, "By age").on_hover_text("Colour by last modified: amber = recent, blue = old");
                ui.separator();
                ui.selectable_value(&mut st.use_alloc, true, "On disk").on_hover_text("Measure by allocated space (what you get back when deleting)");
                ui.selectable_value(&mut st.use_alloc, false, "Size").on_hover_text("Measure by logical file size");
            }
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                if self.tree.is_some() {
                    let st = &mut self.st;
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut st.search_text)
                            .hint_text("🔍 Search all files…   ext:iso size:>1gb older:1y")
                            .desired_width(340.0),
                    );
                    if st.focus_search {
                        r.request_focus();
                        st.focus_search = false;
                    }
                    if r.changed() {
                        st.search_due = Some(Instant::now() + Duration::from_millis(140));
                        if !st.search_text.trim().is_empty() {
                            st.tab = Tab::Search;
                        }
                    }
                }
            });
        });
    }

    fn status_bar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            if let Some((msg, col)) = &self.st.notice {
                ui.label(RichText::new(msg).color(*col));
                if ui.small_button("✕").clicked() {
                    self.st.notice = None;
                }
                ui.separator();
            }
            if let Some(t) = &self.tree {
                let r = t.node(t.root);
                let (icon, col) = match t.method {
                    ScanMethod::Mft => ("⚡", OK),
                    ScanMethod::Walk => ("🐢", WARN),
                    ScanMethod::Ssh => ("🌐", ACCENT),
                };
                let host = t.remote.as_ref().map(|c| format!(" · {}", c.target.label())).unwrap_or_default();
                ui.label(RichText::new(format!("{icon} {}{host}", t.method.label())).color(col));
                ui.separator();
                ui.label(format!("{} files · {} folders", fmt_count(r.files as u64), fmt_count(r.dirs as u64)));
                ui.separator();
                ui.label(format!("scanned in {:.2}s", t.scan_ms as f64 / 1000.0));
            }
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                ui.toggle_value(&mut self.st.remote, "🖥 Remote mode").on_hover_text(
                    "Lighter visuals for Remote Desktop: no hover dimming, animations or floating tooltips on the charts.\nTurned on automatically in RDP sessions.",
                );
                ui.separator();
                if self.st.elevated {
                    ui.label(RichText::new("🛡 Administrator").color(OK));
                } else if cfg!(windows) {
                    if ui
                        .button(RichText::new("⚡ Restart as administrator for instant MFT scan").color(WARN))
                        .on_hover_text("Reading the NTFS Master File Table directly requires administrator rights.\nWithout it SizeMatters falls back to a (much slower) folder walk.")
                        .clicked()
                    {
                        let arg = self.tree.as_ref().map(|t| t.name(t.root).to_string()).or_else(|| self.st.drives.get(self.st.drive_pick).map(|d| d.root.display().to_string())).unwrap_or_default();
                        if platform::relaunch_elevated(&format!("\"{}\"", arg.trim_end_matches('\\').to_string() + "\\")) {
                            std::process::exit(0);
                        }
                    }
                } else {
                    ui.label(RichText::new("standard user").weak());
                }
            });
        });
    }

    fn welcome(&mut self, ui: &mut Ui, scan_req: &mut Option<PathBuf>) {
        ui.add_space(30.0);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new("What's using my disk?").size(30.0).strong());
            ui.add_space(4.0);
            ui.label(
                RichText::new("Pick a drive. SizeMatters reads the NTFS Master File Table directly, so millions of files are indexed in seconds.")
                    .size(14.0)
                    .weak(),
            );
            ui.add_space(24.0);
        });
        let card_w = 300.0;
        let avail = ui.available_width();
        let per_row = ((avail / (card_w + 16.0)).floor() as usize).max(1);
        let rows = self.st.drives.chunks(per_row).map(|c| c.to_vec()).collect::<Vec<_>>();
        for row in rows {
            ui.horizontal(|ui| {
                let used_w = row.len() as f32 * (card_w + 16.0);
                ui.add_space(((avail - used_w) / 2.0).max(0.0));
                for d in row {
                    if drive_card(ui, &d, card_w) {
                        *scan_req = Some(d.root.clone());
                    }
                }
            });
            ui.add_space(12.0);
        }
        ui.add_space(12.0);
        ui.vertical_centered(|ui| {
            ui.horizontal(|ui| {
                ui.add_space((ui.available_width() - 440.0).max(0.0) / 2.0);
                if ui.button(RichText::new("📂  Scan a folder…").size(15.0)).clicked()
                    && let Some(p) = rfd::FileDialog::new().pick_folder()
                {
                    *scan_req = Some(p);
                }
                if ui.button(RichText::new("🌐  Scan a Linux server over SSH…").size(15.0)).clicked() {
                    self.ssh_form = Some(self.last_ssh.clone().unwrap_or_default());
                }
            });
            if cfg!(windows) && !self.st.elevated {
                ui.add_space(18.0);
                ui.label(RichText::new("Tip: run as administrator for the instant MFT scan. Without it, a regular folder scan is used.").color(WARN));
            }
        });
    }

    fn scanning_screen(&mut self, ui: &mut Ui, path: &std::path::Path) {
        ui.add_space(ui.available_height() * 0.28);
        ui.vertical_centered(|ui| {
            ui.add(egui::Spinner::new().size(42.0).color(ACCENT));
            ui.add_space(14.0);
            ui.label(RichText::new(format!("Scanning {}", path.display())).size(22.0).strong());
            let done = self.progress.done.load(Ordering::Relaxed);
            let total = self.progress.total.load(Ordering::Relaxed);
            let el = self.scan_started.elapsed().as_secs_f32();
            ui.add_space(8.0);
            if total > 0 {
                ui.label(RichText::new("Reading the Master File Table…").weak());
                ui.add(egui::ProgressBar::new(done as f32 / total as f32).desired_width(420.0).text(format!("{} / {} records", fmt_count(done), fmt_count(total))));
            } else {
                ui.label(RichText::new(format!("{} items found", fmt_count(done))).weak());
            }
            ui.label(RichText::new(format!("{el:.1}s")).weak());
            ui.add_space(14.0);
            if ui.button("Cancel").clicked() {
                self.progress.cancel.store(true, Ordering::Relaxed);
            }
        });
    }
}

fn drive_label(d: &DriveInfo) -> String {
    let mut s = d.root.display().to_string();
    if !d.label.is_empty() {
        s += &format!("  {}", d.label);
    }
    if d.total > 0 {
        s += &format!("  ({} free)", fmt_size(d.free));
    }
    s
}

fn drive_card(ui: &mut Ui, d: &DriveInfo, w: f32) -> bool {
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 118.0), Sense::click());
    let p = ui.painter();
    let hover = resp.hovered();
    p.rect(rect, 12.0, if hover { Color32::from_rgb(40, 46, 60) } else { Color32::from_rgb(32, 36, 46) }, Stroke::new(1.0, if hover { ACCENT } else { Color32::from_rgb(50, 56, 70) }), StrokeKind::Inside);
    let x = rect.left() + 16.0;
    p.text(pos2(x, rect.top() + 14.0), Align2::LEFT_TOP, format!("💽 {}", d.root.display()), FontId::proportional(22.0), Color32::WHITE);
    let sub = [d.label.as_str(), d.fs.as_str()].iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join(" · ");
    p.text(pos2(x + 110.0, rect.top() + 21.0), Align2::LEFT_TOP, sub, FontId::proportional(13.0), Color32::from_gray(170));
    let used = d.total.saturating_sub(d.free);
    let frac = if d.total > 0 { used as f32 / d.total as f32 } else { 0.0 };
    let bar = egui::Rect::from_min_size(pos2(x, rect.top() + 56.0), vec2(w - 32.0, 12.0));
    p.rect_filled(bar, 6.0, Color32::from_rgb(22, 24, 30));
    let col = if frac > 0.9 { DANGER } else if frac > 0.75 { WARN } else { ACCENT };
    p.rect_filled(egui::Rect::from_min_size(bar.min, vec2(bar.width() * frac, bar.height())), 6.0, col);
    p.text(
        pos2(x, rect.top() + 78.0),
        Align2::LEFT_TOP,
        format!("{} free of {}", fmt_size(d.free), fmt_size(d.total)),
        FontId::proportional(13.0),
        Color32::from_gray(200),
    );
    p.text(pos2(rect.right() - 16.0, rect.bottom() - 14.0), Align2::RIGHT_BOTTOM, if d.fs.eq_ignore_ascii_case("NTFS") { "⚡ instant scan" } else { "" }, FontId::proportional(12.0), OK);
    resp.clicked()
}

// ======================================================================
// Keyboard & search
// ======================================================================

fn handle_keys(ctx: &egui::Context, t: &Tree, st: &mut State, acts: &mut Vec<Action>) {
    let typing = ctx.memory(|m| m.focused().is_some());
    ctx.input(|i| {
        if i.modifiers.command && i.key_pressed(Key::F) {
            st.focus_search = true;
        }
        if typing || st.confirm.is_some() {
            return;
        }
        if i.key_pressed(Key::Backspace) || (i.modifiers.alt && i.key_pressed(Key::ArrowUp)) {
            acts.push(Action::ZoomOut);
        }
        if let Some(s) = st.selected {
            if i.key_pressed(Key::Enter) && t.node(s).is_dir() {
                acts.push(Action::Zoom(s));
            }
            if i.key_pressed(Key::Delete) {
                acts.push(Action::Delete(vec![s], if i.modifiers.shift { Method::Permanent } else { Method::RecycleBin }));
            }
            // arrow navigation in the tree list
            let idx = st.rows.iter().position(|r| r.0 == s);
            if let Some(idx) = idx {
                if i.key_pressed(Key::ArrowDown) && idx + 1 < st.rows.len() {
                    acts.push(Action::TreeSelect(st.rows[idx + 1].0));
                }
                if i.key_pressed(Key::ArrowUp) && !i.modifiers.alt && idx > 0 {
                    acts.push(Action::TreeSelect(st.rows[idx - 1].0));
                }
                if i.key_pressed(Key::ArrowRight) && t.node(s).is_dir() && !st.expanded.contains(&s) {
                    acts.push(Action::Toggle(s));
                }
                if i.key_pressed(Key::ArrowLeft) {
                    if st.expanded.contains(&s) {
                        acts.push(Action::Toggle(s));
                    } else if s != t.root {
                        acts.push(Action::TreeSelect(t.node(s).parent));
                    }
                }
            }
        }
    });
}

fn run_search_if_due(t: &Tree, st: &mut State, ctx: &egui::Context) {
    let Some(due) = st.search_due else { return };
    if Instant::now() < due {
        ctx.request_repaint_after(due - Instant::now());
        return;
    }
    st.search_due = None;
    let q = st.search_text.trim();
    if q.is_empty() {
        st.search_results.clear();
        st.search_err = None;
        return;
    }
    match filter::parse_query(q, st.use_alloc) {
        Ok(c) => {
            st.search_results = filter::search_all(t, &c, 20_000);
            st.search_err = None;
        }
        Err(e) => st.search_err = Some(e),
    }
}

// ======================================================================
// Tree table (WinDirStat-style)
// ======================================================================

const ROW_H: f32 = 22.0;

fn rebuild_rows(t: &Tree, st: &mut State) {
    st.rows.clear();
    let mut stack = vec![(t.root, 0u16)];
    while let Some((i, d)) = stack.pop() {
        st.rows.push((i, d));
        if st.expanded.contains(&i) {
            let kids: Vec<u32> = t.children(i).collect();
            for &c in kids.iter().rev() {
                stack.push((c, d + 1));
            }
        }
    }
    st.rows_dirty = false;
}

fn tree_table(ui: &mut Ui, t: &Tree, st: &mut State, acts: &mut Vec<Action>) {
    if st.rows_dirty {
        rebuild_rows(t, st);
    }
    // header
    // Leave room for the scrollbar so the right-hand columns are never clipped.
    let cols = TreeCols::new((ui.available_width() - 14.0).max(200.0));
    let (hr, _) = ui.allocate_exact_size(vec2(ui.available_width(), 22.0), Sense::hover());
    {
        let p = ui.painter();
        let f = FontId::proportional(12.0);
        let c = Color32::from_gray(150);
        p.text(pos2(hr.left() + 6.0, hr.center().y), Align2::LEFT_CENTER, "Name", f.clone(), c);
        p.text(pos2(hr.left() + cols.bar_x, hr.center().y), Align2::LEFT_CENTER, "% of parent", f.clone(), c);
        p.text(pos2(hr.left() + cols.size_x + cols.size_w, hr.center().y), Align2::RIGHT_CENTER, if st.use_alloc { "On disk" } else { "Size" }, f.clone(), c);
        if cols.files_w > 0.0 {
            p.text(pos2(hr.left() + cols.files_x + cols.files_w, hr.center().y), Align2::RIGHT_CENTER, "Files", f.clone(), c);
        }
        if cols.date_w > 0.0 {
            p.text(pos2(hr.left() + cols.date_x, hr.center().y), Align2::LEFT_CENTER, "Modified", f, c);
        }
    }
    let total_rows = st.rows.len();
    let mut sa = egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt("tree_scroll");
    if st.scroll_to_selected {
        st.scroll_to_selected = false;
        if let Some(sel) = st.selected
            && let Some(idx) = st.rows.iter().position(|r| r.0 == sel)
        {
            let target = idx as f32 * ROW_H - ui.available_height() * 0.4;
            sa = sa.vertical_scroll_offset(target.max(0.0));
        }
    }
    ui.spacing_mut().item_spacing.y = 0.0;
    sa.show_rows(ui, ROW_H, total_rows, |ui, range| {
        for ri in range {
            let (i, depth) = st.rows[ri];
            tree_row(ui, t, st, i, depth, ri, &cols, acts);
        }
    });
}

struct TreeCols {
    bar_x: f32,
    bar_w: f32,
    size_x: f32,
    size_w: f32,
    files_x: f32,
    files_w: f32,
    date_x: f32,
    date_w: f32,
}

impl TreeCols {
    fn new(w: f32) -> Self {
        let date_w = if w > 600.0 { 112.0 } else { 0.0 };
        let files_w = if w > 470.0 { 64.0 } else { 0.0 };
        let size_w = 74.0;
        let bar_w = 74.0;
        let date_x = w - date_w - 4.0;
        let files_x = date_x - files_w - 8.0;
        let size_x = files_x - size_w - 8.0;
        let bar_x = size_x - bar_w - 8.0;
        TreeCols { bar_x, bar_w, size_x, size_w, files_x, files_w, date_x, date_w }
    }
}

#[allow(clippy::too_many_arguments)]
fn tree_row(ui: &mut Ui, t: &Tree, st: &State, i: u32, depth: u16, ri: usize, cols: &TreeCols, acts: &mut Vec<Action>) {
    let n = t.node(i);
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
    let p = ui.painter_at(rect);
    let selected = st.selected == Some(i);
    if selected {
        p.rect_filled(rect, 3.0, Color32::from_rgb(40, 74, 130));
    } else if resp.hovered() || st.hovered == Some(i) {
        p.rect_filled(rect, 3.0, Color32::from_rgb(38, 42, 52));
    } else if ri % 2 == 1 {
        p.rect_filled(rect, 0.0, Color32::from_rgb(27, 29, 36));
    }
    let x0 = rect.left() + 4.0 + depth as f32 * 14.0;
    let cy = rect.center().y;
    let is_dir = n.is_dir();
    let has_kids = is_dir && t.has_children(i);
    let expanded = st.expanded.contains(&i);
    let mut arrow_hit = false;
    if has_kids {
        let ar = egui::Rect::from_center_size(pos2(x0 + 6.0, cy), vec2(14.0, ROW_H));
        draw_arrow(&p, ar.center(), expanded, Color32::from_gray(170));
        if resp.clicked() && ui.input(|inp| inp.pointer.interact_pos()).is_some_and(|pp| ar.contains(pp)) {
            arrow_hit = true;
            acts.push(Action::Toggle(i));
        }
    }
    let icon_col = if is_dir { Color32::from_rgb(240, 196, 90) } else { st.ext_colors.get(n.ext as usize).copied().unwrap_or(Color32::GRAY) };
    draw_icon(&p, pos2(x0 + 22.0, cy), is_dir, n.flags & flags::REPARSE != 0, icon_col);
    let name_max = (rect.left() + cols.bar_x - 8.0) - (x0 + 32.0);
    let name_col = if n.flags & (flags::HIDDEN | flags::SYSTEM | flags::METAFILE) != 0 { Color32::from_gray(150) } else { Color32::from_gray(230) };
    let font = FontId::proportional(13.0);
    let name = t.name(i);
    // clip (cheap) instead of measuring/eliding text every frame
    let name_clip = egui::Rect::from_min_max(pos2(x0 + 30.0, rect.top()), pos2((x0 + 32.0 + name_max).max(x0 + 32.0), rect.bottom()));
    p.with_clip_rect(name_clip.intersect(rect)).text(pos2(x0 + 32.0, cy), Align2::LEFT_CENTER, name, font, name_col);

    // % bar
    let parent_total = if i == t.root {
        n_metric(t, i, st.use_alloc)
    } else {
        n_metric(t, n.parent, st.use_alloc)
    };
    let frac = if parent_total > 0 { n_metric(t, i, st.use_alloc) as f32 / parent_total as f32 } else { 0.0 };
    let bar = egui::Rect::from_min_size(pos2(rect.left() + cols.bar_x, cy - 6.0), vec2(cols.bar_w, 12.0));
    p.rect_filled(bar, 3.0, Color32::from_rgb(20, 22, 28));
    let hue = 0.6 - 0.6 * frac.sqrt();
    p.rect_filled(egui::Rect::from_min_size(bar.min, vec2(cols.bar_w * frac, 12.0)), 3.0, theme::hsv(hue, 0.6, 0.85));
    let pct = format!("{:.1}%", frac * 100.0);
    // Dark text where the (light) fill is behind the label, white over the dark track.
    if frac > 0.5 {
        p.text(bar.center(), Align2::CENTER_CENTER, pct, FontId::proportional(10.5), Color32::from_rgb(16, 18, 22));
    } else {
        p.text(bar.center(), Align2::CENTER_CENTER, pct, FontId::proportional(10.5), Color32::from_gray(235));
    }
    p.text(pos2(rect.left() + cols.size_x + cols.size_w, cy), Align2::RIGHT_CENTER, fmt_size(n_metric(t, i, st.use_alloc)), FontId::monospace(12.0), Color32::from_gray(225));
    if cols.files_w > 0.0 {
        p.text(pos2(rect.left() + cols.files_x + cols.files_w, cy), Align2::RIGHT_CENTER, if is_dir { fmt_count(n.files as u64) } else { String::new() }, FontId::monospace(11.5), Color32::from_gray(160));
    }
    if cols.date_w > 0.0 {
        p.text(pos2(rect.left() + cols.date_x, cy), Align2::LEFT_CENTER, fmt_date(n.modified), FontId::proportional(11.5), Color32::from_gray(150));
    }

    if resp.clicked() && !arrow_hit {
        acts.push(Action::TreeSelect(i));
    }
    if resp.double_clicked() && is_dir {
        acts.push(Action::Zoom(i));
    }
    let resp = hover_tip(resp, st.remote, |ui| node_tooltip(ui, t, i));
    resp.context_menu(|ui| node_menu(ui, t, st, i, acts));
}

/// Display name: the scan root of a folder scan shows only its last component.
fn short_name(t: &Tree, i: u32) -> String {
    let name = t.name(i);
    if i == t.root && name.len() > 3 {
        let trimmed = name.trim_end_matches(['\\', '/']);
        if let Some(p) = trimmed.rfind(['\\', '/']) {
            return trimmed[p + 1..].to_string();
        }
    }
    name.to_string()
}

fn elide(s: &str, max_chars: usize) -> String {
    let max_chars = max_chars.max(4);
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars - 1).collect();
    out.push('…');
    out
}

/// Tooltip that follows the pointer locally, but stays anchored over Remote Desktop
/// (a moving tooltip forces large screen updates on every mouse move).
fn hover_tip(resp: egui::Response, remote: bool, f: impl FnOnce(&mut Ui)) -> egui::Response {
    if remote { resp.on_hover_ui(f) } else { resp.on_hover_ui_at_pointer(f) }
}

/// One-line details for the hovered (or selected) item, under the chart.
fn info_bar(ui: &mut Ui, t: &Tree, st: &State) {
    let Some(i) = st.hovered.or(st.selected).filter(|&i| !t.node(i).is_deleted()) else {
        ui.label(RichText::new("Hover a block for details · double-click to zoom · right-click for actions").weak().size(11.5));
        return;
    };
    let n = t.node(i);
    ui.horizontal(|ui| {
        ui.add(egui::Label::new(RichText::new(t.path(i)).strong().size(12.0)).truncate());
    });
    let mut s = format!("{} on disk · {} size", fmt_size(n.alloc), fmt_size(n.size));
    if n.is_dir() {
        s += &format!(" · {} files, {} folders", fmt_count(n.files as u64), fmt_count(n.dirs as u64));
    }
    s += &format!(" · modified {}", fmt_date(n.modified));
    ui.label(RichText::new(s).size(11.5).color(Color32::from_gray(170)));
}

/// Right-click menu for the chart views. The target is remembered in `st.menu_node`
/// so the menu stays open while the pointer moves onto it.
fn chart_context_menu(resp: &egui::Response, t: &Tree, st: &mut State, acts: &mut Vec<Action>) {
    let Some(node) = st.menu_node.filter(|&n| !t.node(n).is_deleted()) else { return };
    let st_ref: &State = st;
    let shown = resp.context_menu(|ui| node_menu(ui, t, st_ref, node, acts));
    if shown.is_none() && !resp.secondary_clicked() {
        st.menu_node = None; // menu was dismissed
    }
}

/// Expand/collapse triangle, drawn as a shape (no font glyph needed).
fn draw_arrow(p: &egui::Painter, c: Pos2, expanded: bool, col: Color32) {
    let pts = if expanded {
        vec![c + vec2(-4.0, -2.0), c + vec2(4.0, -2.0), c + vec2(0.0, 3.0)]
    } else {
        vec![c + vec2(-2.0, -4.0), c + vec2(3.0, 0.0), c + vec2(-2.0, 4.0)]
    };
    p.add(egui::Shape::convex_polygon(pts, col, Stroke::NONE));
}

/// Folder / file / link icon drawn with shapes, so it renders with any font.
fn draw_icon(p: &egui::Painter, c: Pos2, is_dir: bool, is_link: bool, col: Color32) {
    if is_dir {
        let body = egui::Rect::from_center_size(c + vec2(0.0, 1.0), vec2(14.0, 9.0));
        let tab = egui::Rect::from_min_size(body.left_top() + vec2(0.0, -2.5), vec2(6.0, 3.0));
        if is_link {
            p.rect_stroke(body, 1.5, Stroke::new(1.2, col), StrokeKind::Inside);
            p.rect_stroke(tab, 1.0, Stroke::new(1.2, col), StrokeKind::Inside);
        } else {
            p.rect_filled(tab, 1.0, col.gamma_multiply(0.8));
            p.rect_filled(body, 1.5, col);
        }
    } else {
        let page = egui::Rect::from_center_size(c, vec2(9.0, 12.0));
        if is_link {
            p.rect_stroke(page, 1.0, Stroke::new(1.2, col), StrokeKind::Inside);
        } else {
            p.rect_filled(page, 1.5, col);
            // folded corner
            let tr = page.right_top();
            p.add(egui::Shape::convex_polygon(vec![tr + vec2(-3.5, 0.0), tr, tr + vec2(0.0, 3.5)], Color32::from_rgb(24, 26, 32), Stroke::NONE));
        }
    }
}

#[inline]
fn n_metric(t: &Tree, i: u32, use_alloc: bool) -> u64 {
    let n = t.node(i);
    if use_alloc { n.alloc } else { n.size }
}

fn node_tooltip(ui: &mut Ui, t: &Tree, i: u32) {
    let n = t.node(i);
    ui.label(RichText::new(t.path(i)).strong());
    egui::Grid::new("tt").num_columns(2).spacing([12.0, 2.0]).show(ui, |ui| {
        ui.label("Size");
        ui.label(format!("{} ({} bytes)", fmt_size(n.size), fmt_count(n.size)));
        ui.end_row();
        ui.label("On disk");
        ui.label(fmt_size(n.alloc));
        ui.end_row();
        if n.is_dir() {
            ui.label("Contains");
            ui.label(format!("{} files, {} folders", fmt_count(n.files as u64), fmt_count(n.dirs as u64)));
            ui.end_row();
        }
        ui.label("Modified");
        ui.label(fmt_date(n.modified));
        ui.end_row();
        ui.label("Created");
        ui.label(fmt_date(n.created));
        ui.end_row();
    });
    let mut tags = Vec::new();
    if n.flags & flags::HARDLINK != 0 {
        tags.push("hard link (counted once)");
    }
    if n.flags & flags::OFFLINE != 0 {
        tags.push("cloud placeholder");
    }
    if n.flags & flags::REPARSE != 0 {
        tags.push("link / reparse point");
    }
    if n.flags & flags::COMPRESSED != 0 {
        tags.push("compressed");
    }
    if n.flags & flags::SPARSE != 0 {
        tags.push("sparse");
    }
    if n.flags & flags::METAFILE != 0 {
        tags.push("NTFS metadata");
    }
    if !tags.is_empty() {
        ui.label(RichText::new(tags.join(" · ")).weak());
    }
}

fn node_menu(ui: &mut Ui, t: &Tree, st: &State, i: u32, acts: &mut Vec<Action>) {
    let n = t.node(i);
    ui.label(RichText::new(t.name(i)).strong());
    ui.separator();
    if n.is_dir() && ui.button("🔍 Zoom into folder").clicked() {
        acts.push(Action::Zoom(i));
        ui.close();
    }
    let local = t.remote.is_none();
    if local && ui.button("📂 Show in Explorer").clicked() {
        acts.push(Action::Reveal(i));
        ui.close();
    }
    if local && ui.button("↗ Open").clicked() {
        acts.push(Action::Open(i));
        ui.close();
    }
    if ui.button("📋 Copy path").clicked() {
        acts.push(Action::CopyPath(i));
        ui.close();
    }
    if n.is_dir() && ui.button("🧹 Find & select files in here…").clicked() {
        acts.push(Action::Scope(i));
        ui.close();
    }
    ui.separator();
    let checked = st.checked.contains(&i);
    if ui.button(if checked { "☐ Remove from selection" } else { "☑ Add to delete selection" }).clicked() {
        acts.push(Action::Check(i, !checked));
        ui.close();
    }
    if i != t.root {
        if local && ui.button(RichText::new("🗑 Move to Recycle Bin…").color(WARN)).clicked() {
            acts.push(Action::Delete(vec![i], Method::RecycleBin));
            ui.close();
        }
        if ui.button(RichText::new("⚠ Delete permanently…").color(DANGER)).clicked() {
            acts.push(Action::Delete(vec![i], Method::Permanent));
            ui.close();
        }
    }
}

// ======================================================================
// Center: breadcrumb + treemap / sunburst
// ======================================================================

fn center_view(ui: &mut Ui, t: &Tree, st: &mut State, acts: &mut Vec<Action>) {
    // breadcrumb
    ui.horizontal(|ui| {
        if ui.add_enabled(st.view_root != t.root, egui::Button::new("⬅")).on_hover_text("Up one level (Backspace)").clicked() {
            acts.push(Action::ZoomOut);
        }
        let chain = t.ancestry(st.view_root);
        for (k, &c) in chain.iter().enumerate() {
            if k > 0 {
                ui.label(RichText::new("›").weak());
            }
            let label = if c == t.root { elide(t.name(c), 48) } else { t.name(c).to_string() };
            let txt = RichText::new(label).color(if c == st.view_root { Color32::WHITE } else { ACCENT });
            if ui.add(egui::Button::new(txt).frame(false)).clicked() {
                acts.push(Action::Zoom(c));
            }
        }
        ui.label(RichText::new(format!("  {}", fmt_size(n_metric(t, st.view_root, st.use_alloc)))).weak());
        if let Some(h) = st.highlight_ext {
            ui.separator();
            let e = &t.exts[h as usize];
            if ui.button(format!("Highlighting .{}  ✕", if e.name.is_empty() { "(none)" } else { &e.name })).clicked() {
                acts.push(Action::HighlightExt(None));
            }
        }
    });
    ui.add_space(4.0);
    category_bar(ui, t, st);
    ui.add_space(8.0);
    egui::Panel::bottom("hover_info").frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(2, 4))).show(ui, |ui| info_bar(ui, t, st));
    match st.view {
        View::Strata => strata::view(ui, t, st, acts),
        View::Treemap => treemap_view(ui, t, st, acts),
    }
}

pub(crate) fn cat_color(c: Category) -> Color32 {
    let (l, ch, h) = match c {
        Category::Videos => (0.66, 0.16, 25.0),
        Category::Images => (0.72, 0.15, 145.0),
        Category::Music => (0.68, 0.15, 305.0),
        Category::Documents => (0.70, 0.12, 245.0),
        Category::Archives => (0.78, 0.14, 80.0),
        Category::Apps => (0.68, 0.12, 200.0),
        Category::Code => (0.76, 0.13, 115.0),
        Category::Games => (0.64, 0.17, 345.0),
        Category::System => (0.55, 0.03, 255.0),
        Category::Other => (0.64, 0.02, 255.0),
    };
    strata::oklch(l, ch, h)
}

fn file_cat_color(t: &Tree, _st: &State, cats: &[Category], i: u32) -> Color32 {
    cat_color(category::of_node(t, cats, i))
}

/// Phone-style storage bar: what kind of data fills the current folder.
fn category_bar(ui: &mut Ui, t: &Tree, st: &mut State) {
    let key = (st.view_root, st.generation);
    if st.cat_cache.as_ref().is_none_or(|c| c.0 != key) {
        st.cat_cache = Some((key, category::breakdown(t, &st.cats, st.view_root)));
    }
    let data = st.cat_cache.as_ref().unwrap().1;
    let metric = |d: &(u64, u64, u64)| if st.use_alloc { d.1 } else { d.0 };
    let total: u64 = data.iter().map(metric).sum::<u64>().max(1);
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 14.0), Sense::click());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 7.0, Color32::from_rgb(30, 33, 40));
    let mut x = rect.left();
    let mut hover_cat = None;
    for c in category::ALL {
        let v = metric(&data[c.index()]);
        if v == 0 {
            continue;
        }
        let w = rect.width() * v as f32 / total as f32;
        let r = egui::Rect::from_min_size(pos2(x, rect.top()), vec2(w, rect.height()));
        let mut col = cat_color(c);
        if st.highlight_cat.is_some_and(|h| h != c) {
            col = col.gamma_multiply(0.3);
        }
        p.rect_filled(r, 0.0, col);
        if resp.hover_pos().is_some_and(|hp| r.contains(hp)) {
            hover_cat = Some((c, v));
        }
        x += w;
    }
    // round the ends by redrawing the frame corners
    p.rect_stroke(rect, 7.0, Stroke::new(2.0, Color32::from_rgb(18, 20, 24)), StrokeKind::Outside);
    if let Some((c, v)) = hover_cat {
        let resp = resp.on_hover_text(format!("{} {} · {} · {:.1}%", c.icon(), c.label(), fmt_size(v), v as f64 * 100.0 / total as f64));
        if resp.clicked() {
            st.highlight_cat = if st.highlight_cat == Some(c) { None } else { Some(c) };
        }
    }
    ui.add_space(3.0);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 10.0;
        for c in category::ALL {
            let v = metric(&data[c.index()]);
            if v == 0 {
                continue;
            }
            let on = st.highlight_cat == Some(c);
            let dim = st.highlight_cat.is_some() && !on;
            let col = cat_color(c);
            let mut job = egui::text::LayoutJob::default();
            job.append("● ", 0.0, egui::TextFormat::simple(FontId::proportional(12.0), if dim { col.gamma_multiply(0.35) } else { col }));
            job.append(
                &format!("{} {}", c.label(), fmt_size(v)),
                0.0,
                egui::TextFormat::simple(FontId::proportional(11.5), if on { Color32::WHITE } else if dim { Color32::from_gray(110) } else { Color32::from_gray(190) }),
            );
            if ui.add(egui::Label::new(job).sense(Sense::click())).on_hover_text("Click to highlight this kind of file").clicked() {
                st.highlight_cat = if on { None } else { Some(c) };
            }
        }
        if st.view_root == t.root && st.use_alloc && t.volume_total > 0 && platform::drive_letter_root(std::path::Path::new(t.name(t.root))).is_some() {
            ui.checkbox(&mut st.show_free, RichText::new("show free space").size(11.5));
        }
    });
}

fn file_color(t: &Tree, st: &State, i: u32) -> Color32 {
    let n = t.node(i);
    let c = match st.color_mode {
        ColorMode::Folder => st.ext_colors.get(n.ext as usize).copied().unwrap_or(Color32::GRAY),
        ColorMode::Type => cat_color(category::of_node(t, &st.cats, i)),
        ColorMode::Age => theme::age_color(n.modified, st.now),
    };
    if let Some(hc) = st.highlight_cat
        && category::of_node(t, &st.cats, i) != hc
    {
        return Color32::from_rgb(60, 62, 70);
    }
    match st.highlight_ext {
        Some(h) if h != n.ext => Color32::from_rgb(60, 62, 70),
        _ => c,
    }
}

fn treemap_view(ui: &mut Ui, t: &Tree, st: &mut State, acts: &mut Vec<Action>) {
    let rect = ui.available_rect_before_wrap();
    let resp = ui.allocate_rect(rect, Sense::click());
    let ppp = ui.ctx().pixels_per_point();
    let (w, h) = ((rect.width() * ppp) as usize, (rect.height() * ppp) as usize);
    if w < 8 || h < 8 {
        return;
    }
    let key = (st.view_root, w, h, st.use_alloc, st.color_mode, st.generation, st.highlight_ext, st.highlight_cat);
    if st.tm.as_ref().is_none_or(|c| c.key != key) {
        let layout = treemap::layout(t, st.view_root, w, h, &treemap::Params { use_alloc: st.use_alloc, headers: true, max_cells: 250_000 });
        let st_ref: &State = st;
        let img = treemap::rasterize(&layout, t, true, &|i| file_color(t, st_ref, i));
        let tex = match st.tm.take() {
            Some(mut c) => {
                c.tex.set(img, egui::TextureOptions::NEAREST);
                c.tex
            }
            None => ui.ctx().load_texture("treemap", img, egui::TextureOptions::NEAREST),
        };
        st.tm = Some(TmCache { key, layout, tex });
    }
    let cache = st.tm.as_ref().unwrap();
    let painter = ui.painter_at(rect);
    painter.image(cache.tex.id(), rect, egui::Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);

    let to_screen = |r: &treemap::Rect| egui::Rect::from_min_max(pos2(rect.left() + r.x0 / ppp, rect.top() + r.y0 / ppp), pos2(rect.left() + r.x1 / ppp, rect.top() + r.y1 / ppp));

    // labels
    let mut labels = 0;
    for c in &cache.layout.cells {
        if labels > 900 {
            break;
        }
        let r = to_screen(&c.rect);
        if c.header {
            let txt = format!("{}  {}", t.name(c.node), fmt_size(n_metric(t, c.node, st.use_alloc)));
            let hr = egui::Rect::from_min_size(r.min, vec2(r.width(), treemap::HEADER_H / ppp * ppp.min(1.0).max(1.0)));
            painter.with_clip_rect(hr.intersect(rect)).text(pos2(r.left() + 4.0, r.top() + 1.0), Align2::LEFT_TOP, txt, FontId::proportional(11.0), Color32::from_gray(235));
            labels += 1;
        } else if !c.is_dir && r.width() > 70.0 && r.height() > 30.0 {
            let clip = painter.with_clip_rect(r.shrink(2.0).intersect(rect));
            let name = t.name(c.node);
            let cpos = r.center();
            clip.text(cpos + vec2(1.0, 1.0) - vec2(0.0, 7.0), Align2::CENTER_CENTER, name, FontId::proportional(12.0), Color32::from_black_alpha(160));
            clip.text(cpos - vec2(0.0, 7.0), Align2::CENTER_CENTER, name, FontId::proportional(12.0), Color32::WHITE);
            clip.text(cpos + vec2(0.0, 8.0), Align2::CENTER_CENTER, fmt_size(n_metric(t, c.node, st.use_alloc)), FontId::proportional(11.0), Color32::from_white_alpha(220));
            labels += 1;
        }
    }

    // selection outline
    if let Some(sel) = st.selected
        && let Some(c) = cache.layout.cells.iter().find(|c| c.node == sel)
    {
        painter.rect_stroke(to_screen(&c.rect), 0.0, Stroke::new(2.5, Color32::WHITE), StrokeKind::Inside);
        painter.rect_stroke(to_screen(&c.rect).shrink(2.5), 0.0, Stroke::new(1.0, Color32::BLACK), StrokeKind::Inside);
    }

    let prev_hover = st.hovered;
    st.hovered = None;
    if let Some(pos) = resp.hover_pos() {
        let (x, y) = ((pos.x - rect.left()) * ppp, (pos.y - rect.top()) * ppp);
        if let Some(c) = treemap::hit(&cache.layout, x, y) {
            let node = c.node;
            let r = to_screen(&c.rect);
            painter.rect_stroke(r, 0.0, Stroke::new(1.5, Color32::from_white_alpha(200)), StrokeKind::Inside);
            st.hovered = Some(node);
            // folder this file sits in
            let parent = t.node(node).parent;
            if let Some(pc) = cache.layout.cells.iter().find(|c| c.node == parent && parent != st.view_root) {
                painter.rect_stroke(to_screen(&pc.rect), 0.0, Stroke::new(1.0, ACCENT), StrokeKind::Inside);
            }
            // Remote mode: details go to the info bar instead of a tooltip chasing the mouse.
            let resp = if st.remote { resp.clone() } else { resp.clone().on_hover_ui_at_pointer(|ui| node_tooltip(ui, t, node)) };
            if resp.clicked() {
                acts.push(Action::Select(node));
            }
            if resp.double_clicked() {
                let target = if t.node(node).is_dir() { node } else { parent };
                if target != NONE {
                    acts.push(Action::Zoom(target));
                }
            }
            if resp.secondary_clicked() {
                acts.push(Action::Select(node));
                st.menu_node = Some(node);
            }
        }
    }
    chart_context_menu(&resp, t, st, acts);
    if st.hovered != prev_hover {
        ui.ctx().request_repaint();
    }
    if resp.clicked_by(egui::PointerButton::Extra1) {
        acts.push(Action::ZoomOut);
    }
}

// ======================================================================
// Side panel
// ======================================================================

fn side_panel(ui: &mut Ui, t: &Tree, st: &mut State, acts: &mut Vec<Action>) {
    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        ui.selectable_value(&mut st.tab, Tab::Overview, "📊 Overview");
        ui.selectable_value(&mut st.tab, Tab::Cleanup, "🧹 Clean up");
        let sl = if st.search_results.is_empty() { "🔍 Search".to_string() } else { format!("🔍 Search ({})", fmt_count(st.search_results.len() as u64)) };
        ui.selectable_value(&mut st.tab, Tab::Search, sl);
        ui.selectable_value(&mut st.tab, Tab::Largest, "🏆 Largest");
        ui.selectable_value(&mut st.tab, Tab::Dupes, "👯 Duplicates");
        ui.selectable_value(&mut st.tab, Tab::Changes, "📈 Changes");
    });
    ui.separator();

    // selection bar pinned to the bottom
    egui::Panel::bottom("selbar").frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(0, 6))).show(ui, |ui| {
        selection_bar(ui, t, st, acts);
    });

    match st.tab {
        Tab::Overview => overview(ui, t, st, acts),
        Tab::Cleanup => cleanup(ui, t, st, acts),
        Tab::Search => {
            if let Some(e) = &st.search_err {
                ui.colored_label(DANGER, e);
            }
            if st.search_text.trim().is_empty() {
                ui.label(RichText::new("Type in the search box at the top right. Everything on the drive is searched instantly.").weak());
                ui.add_space(6.0);
                ui.label(RichText::new("Examples").strong());
                for (q, d) in [
                    ("report", "name contains 'report'"),
                    ("*.iso", "wildcards"),
                    ("ext:mp4;mkv size:>500mb", "big videos"),
                    ("older:2y ext:zip", "zips untouched for 2 years"),
                    ("folder: node_modules", "all node_modules folders"),
                    ("path:downloads newer:7d", "recent downloads"),
                    ("regex:^IMG_\\d{4}", "regular expression"),
                ] {
                    ui.horizontal(|ui| {
                        if ui.link(RichText::new(q).monospace()).clicked() {
                            st.search_text = q.to_string();
                            st.search_due = Some(Instant::now());
                        }
                        ui.label(RichText::new(d).weak());
                    });
                }
            } else {
                let ids = std::mem::take(&mut st.search_results);
                results_table(ui, t, st, &ids, "search", acts);
                st.search_results = ids;
            }
        }
        Tab::Dupes => dupes_tab(ui, t, st, acts),
        Tab::Changes => changes_tab(ui, t, st, acts),
        Tab::Largest => {
            let key = (st.view_root, st.generation, st.use_alloc);
            if st.largest.as_ref().is_none_or(|c| c.0 != key) {
                let c = filter::parse_query("file:", st.use_alloc).unwrap();
                st.largest = Some((key, filter::run(t, st.view_root, &c, 1000)));
            }
            ui.add(egui::Label::new(RichText::new(format!("Largest files in {}", t.path(st.view_root))).weak()).truncate());
            let (key, ids) = st.largest.take().unwrap();
            results_table(ui, t, st, &ids, "largest", acts);
            st.largest = Some((key, ids));
        }
    }
}

fn dupes_tab(ui: &mut Ui, t: &Tree, st: &mut State, acts: &mut Vec<Action>) {
    if t.remote.is_some() {
        ui.label(RichText::new("Duplicate search reads file contents, so it is available for local scans only.").weak());
        return;
    }
    // poll background job
    if let Some((_, rx, _)) = &st.dupes_job
        && let Ok(mut groups) = rx.try_recv()
    {
        for g in groups.iter_mut() {
            g.files.sort_by_key(|&i| std::cmp::Reverse(t.node(i).modified));
        }
        st.dupes = Some(groups);
        st.dupes_job = None;
    }
    ui.label(RichText::new(format!("Find identical files in {}", short_name(t, st.view_root))).weak());
    ui.horizontal(|ui| {
        ui.label("Min size");
        ui.add(egui::TextEdit::singleline(&mut st.dupes_min).desired_width(70.0));
        let busy = st.dupes_job.is_some();
        if ui.add_enabled(!busy, egui::Button::new(RichText::new("👯 Find duplicates").strong()).fill(Color32::from_rgb(40, 90, 170))).clicked() {
            let min = filter::parse_size(&st.dupes_min).ok().flatten().unwrap_or(1 << 20);
            let cands = crate::dupes::candidates(t, st.view_root, min);
            let progress = Arc::new(crate::dupes::Progress::default());
            let (tx, rx) = crossbeam_channel::bounded(1);
            let (p2, ctx) = (progress.clone(), ui.ctx().clone());
            std::thread::spawn(move || {
                let g = crate::dupes::hash_groups(cands, &p2);
                let _ = tx.send(g);
                ctx.request_repaint();
            });
            st.dupes_job = Some((progress, rx, Instant::now()));
            st.dupes = None;
        }
    });
    if let Some((p, _, started)) = &st.dupes_job {
        let (d, total) = (p.done.load(Ordering::Relaxed), p.total.load(Ordering::Relaxed).max(1));
        let label = match p.stage.load(Ordering::Relaxed) {
            3 => format!("comparing contents {} / {}", fmt_size(d), fmt_size(total)),
            _ => format!("quick check {d} / {total} files"),
        };
        ui.add(egui::ProgressBar::new(d as f32 / total as f32).text(label));
        ui.label(RichText::new(format!("{:.1}s", started.elapsed().as_secs_f32())).weak());
        if ui.button("Cancel").clicked() {
            p.cancel.store(true, Ordering::Relaxed);
        }
        ui.ctx().request_repaint_after(Duration::from_millis(150));
        return;
    }
    let Some(groups) = &st.dupes else {
        ui.label(RichText::new("Files are grouped by size first, then checked by a quick head/tail hash, then a full content hash — only likely duplicates are read.").weak().size(11.5));
        return;
    };
    let live: Vec<&crate::dupes::Group> = groups.iter().filter(|g| g.files.iter().filter(|&&i| !t.node(i).is_deleted()).count() > 1).collect();
    let wasted: u64 = live.iter().map(|g| g.size * (g.files.iter().filter(|&&i| !t.node(i).is_deleted()).count() as u64 - 1)).sum();
    ui.label(RichText::new(format!("{} groups · {} reclaimable", fmt_count(live.len() as u64), fmt_size(wasted))).strong());
    ui.horizontal_wrapped(|ui| {
        ui.label("Select all copies except the");
        for (lbl, k) in [("newest", crate::dupes::Keep::Newest), ("oldest", crate::dupes::Keep::Oldest), ("shortest path", crate::dupes::Keep::ShortestPath)] {
            if ui.small_button(lbl).clicked() {
                let owned: Vec<crate::dupes::Group> = live.iter().map(|g| (*g).clone()).collect();
                acts.push(Action::CheckMany(crate::dupes::auto_select(t, &owned, k), true));
            }
        }
    });
    ui.separator();
    // flatten groups into rows: header + files
    let mut rows: Vec<(Option<usize>, u32)> = Vec::new(); // (group idx for header, node)
    for (gi, g) in live.iter().enumerate().take(2000) {
        rows.push((Some(gi), NONE));
        for &f in &g.files {
            if !t.node(f).is_deleted() {
                rows.push((None, f));
            }
        }
    }
    ui.spacing_mut().item_spacing.y = 0.0;
    egui::ScrollArea::vertical().id_salt("dupes").auto_shrink([false, false]).show_rows(ui, 32.0, rows.len(), |ui, range| {
        for &(hdr, f) in &rows[range] {
            let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 32.0), Sense::click());
            let p = ui.painter_at(rect);
            if let Some(gi) = hdr {
                let g = live[gi];
                let n = g.files.iter().filter(|&&i| !t.node(i).is_deleted()).count();
                p.rect_filled(rect.shrink2(vec2(0.0, 2.0)), 4.0, Color32::from_rgb(36, 40, 52));
                p.text(pos2(rect.left() + 8.0, rect.center().y), Align2::LEFT_CENTER, format!("{n} × {}   ·   {} wasted", fmt_size(g.size), fmt_size(g.size * (n as u64 - 1))), FontId::proportional(12.0), Color32::from_gray(210));
                continue;
            }
            let checked = st.checked.contains(&f);
            if resp.hovered() {
                p.rect_filled(rect, 3.0, Color32::from_rgb(38, 42, 52));
            }
            let cb = egui::Rect::from_center_size(pos2(rect.left() + 18.0, rect.center().y), vec2(13.0, 13.0));
            p.rect(cb, 3.0, if checked { ACCENT } else { Color32::from_rgb(30, 33, 40) }, Stroke::new(1.0, Color32::from_gray(120)), StrokeKind::Inside);
            if checked {
                p.text(cb.center(), Align2::CENTER_CENTER, "✔", FontId::proportional(10.0), Color32::WHITE);
            }
            let clip = p.with_clip_rect(egui::Rect::from_min_max(rect.min, pos2(rect.right() - 110.0, rect.bottom())));
            clip.text(pos2(rect.left() + 32.0, rect.top() + 3.0), Align2::LEFT_TOP, t.name(f), FontId::proportional(12.5), if checked { Color32::from_rgb(255, 170, 150) } else { Color32::from_gray(225) });
            // folder relative to the current view
            let base = t.path(st.view_root);
            let full = t.path(t.node(f).parent);
            let rel = full.strip_prefix(&base).unwrap_or(&full).trim_start_matches(['\\', '/']);
            clip.text(pos2(rect.left() + 32.0, rect.bottom() - 3.0), Align2::LEFT_BOTTOM, if rel.is_empty() { "." } else { rel }, FontId::proportional(10.5), Color32::from_gray(130));
            p.text(pos2(rect.right() - 4.0, rect.center().y), Align2::RIGHT_CENTER, fmt_date(t.node(f).modified), FontId::proportional(11.0), Color32::from_gray(140));
            if resp.clicked() {
                acts.push(Action::Check(f, !checked));
            }
            resp.context_menu(|ui| node_menu(ui, t, st, f, acts));
        }
    });
}

fn changes_tab(ui: &mut Ui, t: &Tree, st: &mut State, acts: &mut Vec<Action>) {
    if st.viewing_saved.is_some() {
        ui.label(RichText::new("You are viewing a saved scan. Rescan the location to compare it with history.").weak());
        return;
    }
    let label = crate::snapshot::label_of(t);
    if st.history.as_ref().is_none_or(|h| h.0 != label) {
        st.history = Some((label.clone(), crate::snapshot::history(&label)));
    }
    // poll loading
    if let Some(rx) = &st.changes_loading
        && let Ok(res) = rx.try_recv()
    {
        st.changes_loading = None;
        match res {
            Ok((old, meta)) => {
                let min = filter::parse_size(&st.changes_min).ok().flatten().unwrap_or(10 << 20);
                let ch = crate::snapshot::grew(&old, t, min, 2000);
                st.changes = Some((meta, ch));
                st.changes_old = Some(Box::new(old));
            }
            Err(e) => st.notice = Some((e, DANGER)),
        }
    }
    ui.label(RichText::new("What grew since an earlier scan of this location? Every scan is kept automatically (last 5).").weak().size(11.5));
    let hist = st.history.as_ref().unwrap().1.clone();
    // only scans taken before the current one (the current scan is saved too)
    let earlier: Vec<&(PathBuf, crate::snapshot::Meta)> = hist.iter().filter(|(_, m)| m.taken < st.now).collect();
    ui.horizontal_wrapped(|ui| {
        ui.label("Min change");
        if ui.add(egui::TextEdit::singleline(&mut st.changes_min).desired_width(70.0)).lost_focus()
            && let Some(old) = &st.changes_old
        {
            let min = filter::parse_size(&st.changes_min).ok().flatten().unwrap_or(10 << 20);
            let meta = st.changes.as_ref().map(|c| c.0.clone());
            if let Some(meta) = meta {
                st.changes = Some((meta, crate::snapshot::grew(old, t, min, 2000)));
            }
        }
        if ui.button("Compare with a file…").clicked()
            && let Some(p) = rfd::FileDialog::new().add_filter("SizeMatters scan", &["smscan"]).pick_file()
        {
            st.changes_loading = Some(load_async(p, ui.ctx()));
        }
    });
    if earlier.is_empty() {
        ui.label(RichText::new("No earlier scans of this location yet — scan it again later (or open a saved scan) to see what changed.").weak());
    } else {
        ui.label("Compare with:");
        for (p, m) in earlier.iter().take(4) {
            if ui.button(format!("{}  ·  {} files  ·  {}", fmt_date(m.taken), fmt_count(m.files), fmt_size(m.alloc))).clicked() {
                st.changes_loading = Some(load_async(p.clone(), ui.ctx()));
            }
        }
    }
    if st.changes_loading.is_some() {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Loading earlier scan…");
        });
        ui.ctx().request_repaint_after(Duration::from_millis(150));
    }
    ui.separator();
    let Some((meta, ch)) = &st.changes else { return };
    let grown: i64 = ch.iter().filter(|c| c.now.is_some_and(|n| t.node(n).parent == t.root) || c.now.is_none() && c.path.matches(t.sep).count() <= 1).map(|c| c.delta()).sum();
    let total_delta = t.node(t.root).alloc as i64 - meta.alloc as i64;
    ui.label(RichText::new(format!("Since {}: {}{} overall", fmt_date(meta.taken), if total_delta >= 0 { "+" } else { "−" }, fmt_size(total_delta.unsigned_abs()))).strong());
    let _ = grown;
    ui.spacing_mut().item_spacing.y = 0.0;
    let ch = ch.clone();
    egui::ScrollArea::vertical().id_salt("changes").auto_shrink([false, false]).show_rows(ui, 34.0, ch.len(), |ui, range| {
        for c in &ch[range] {
            let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 34.0), Sense::click());
            let p = ui.painter_at(rect);
            if resp.hovered() {
                p.rect_filled(rect, 3.0, Color32::from_rgb(38, 42, 52));
            }
            let d = c.delta();
            let (col, sign) = if d >= 0 { (Color32::from_rgb(255, 150, 120), "+") } else { (OK, "−") };
            let tag = if c.before == 0 { "new" } else if c.after == 0 { "removed" } else { "" };
            let clip = p.with_clip_rect(egui::Rect::from_min_max(rect.min, pos2(rect.right() - 100.0, rect.bottom())));
            let root_path = t.name(t.root);
            let shown = c.path.strip_prefix(root_path).unwrap_or(&c.path).trim_start_matches(['\\', '/']);
            clip.text(pos2(rect.left() + 6.0, rect.top() + 4.0), Align2::LEFT_TOP, format!("{}{}", if c.is_dir { "📁 " } else { "" }, shown), FontId::proportional(12.5), Color32::from_gray(225));
            clip.text(pos2(rect.left() + 6.0, rect.bottom() - 4.0), Align2::LEFT_BOTTOM, format!("{} → {}  {tag}", fmt_size(c.before), fmt_size(c.after)), FontId::proportional(10.5), Color32::from_gray(140));
            p.text(pos2(rect.right() - 6.0, rect.center().y), Align2::RIGHT_CENTER, format!("{sign}{}", fmt_size(d.unsigned_abs())), FontId::monospace(12.5), col);
            if resp.clicked()
                && let Some(n) = c.now
                && !t.node(n).is_deleted()
            {
                acts.push(Action::Select(n));
            }
        }
    });
}

fn load_async(p: PathBuf, ctx: &egui::Context) -> crossbeam_channel::Receiver<Result<(Tree, crate::snapshot::Meta), String>> {
    let (tx, rx) = crossbeam_channel::bounded(1);
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        let _ = tx.send(crate::snapshot::load(&p).map_err(|e| e.to_string()));
        ctx.request_repaint();
    });
    rx
}

fn overview(ui: &mut Ui, t: &Tree, st: &mut State, acts: &mut Vec<Action>) {
    let r = t.node(t.root);
    egui::ScrollArea::vertical().id_salt("ov").show(ui, |ui| {
        if t.volume_total > 0 {
            ui.label(RichText::new("Volume").strong());
            let used = t.volume_total.saturating_sub(t.volume_free);
            let whole_drive = t.method == ScanMethod::Mft || platform::drive_letter_root(std::path::Path::new(t.name(t.root))).is_some();
            let (w, h) = (ui.available_width(), 18.0);
            let (rect, _) = ui.allocate_exact_size(vec2(w, h), Sense::hover());
            let p = ui.painter();
            p.rect_filled(rect, 5.0, Color32::from_rgb(40, 44, 54));
            let scanned = r.alloc.min(used);
            let fs = scanned as f32 / t.volume_total as f32;
            let fu = used as f32 / t.volume_total as f32;
            p.rect_filled(egui::Rect::from_min_size(rect.min, vec2(w * fu, h)), 5.0, Color32::from_rgb(120, 90, 160));
            p.rect_filled(egui::Rect::from_min_size(rect.min, vec2(w * fs, h)), 5.0, ACCENT);
            ui.horizontal_wrapped(|ui| {
                legend_dot(ui, ACCENT, &format!("Scanned {}", fmt_size(scanned)));
                if whole_drive {
                    legend_dot(ui, Color32::from_rgb(120, 90, 160), &format!("Unaccounted {}", fmt_size(used.saturating_sub(scanned))))
                        .on_hover_text("Used space not attributable to visible files: shadow copies / restore points, files you lack access to, NTFS overhead.");
                } else {
                    legend_dot(ui, Color32::from_rgb(120, 90, 160), &format!("Elsewhere {}", fmt_size(used.saturating_sub(scanned))));
                }
                legend_dot(ui, Color32::from_rgb(40, 44, 54), &format!("Free {}", fmt_size(t.volume_free)));
            });
            ui.label(RichText::new(format!("Capacity {} · used {:.1}%", fmt_size(t.volume_total), fu * 100.0)).weak());
            ui.add_space(10.0);
        }

        ui.label(RichText::new("File types").strong());
        ui.label(RichText::new("Click a type to highlight it in the map").weak().size(11.0));
        let okey = (st.generation, st.use_alloc);
        if st.ext_order.as_ref().is_none_or(|o| o.0 != okey) {
            let mut order: Vec<usize> = (0..t.exts.len()).filter(|&k| t.exts[k].count > 0).collect();
            order.sort_by(|&a, &b| {
                let (ea, eb) = (&t.exts[a], &t.exts[b]);
                if st.use_alloc { eb.alloc.cmp(&ea.alloc) } else { eb.size.cmp(&ea.size) }
            });
            order.truncate(60);
            st.ext_order = Some((okey, order));
        }
        let order = st.ext_order.as_ref().unwrap().1.clone();
        let total = if st.use_alloc { r.alloc } else { r.size }.max(1);
        for &k in order.iter().take(60) {
            let e = &t.exts[k];
            let v = if st.use_alloc { e.alloc } else { e.size };
            let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 22.0), Sense::click());
            let p = ui.painter_at(rect);
            let hl = st.highlight_ext == Some(k as u16);
            if hl {
                p.rect_filled(rect, 3.0, Color32::from_rgb(40, 74, 130));
            } else if resp.hovered() {
                p.rect_filled(rect, 3.0, Color32::from_rgb(38, 42, 52));
            }
            let col = st.ext_colors.get(k).copied().unwrap_or(Color32::GRAY);
            p.rect_filled(egui::Rect::from_center_size(pos2(rect.left() + 10.0, rect.center().y), vec2(12.0, 12.0)), 3.0, col);
            let label = if e.name.is_empty() { "(no extension)".to_string() } else { format!(".{}", e.name) };
            p.text(pos2(rect.left() + 24.0, rect.center().y), Align2::LEFT_CENTER, label, FontId::proportional(12.5), Color32::from_gray(230));
            let frac = v as f32 / total as f32;
            let bx = rect.left() + rect.width() * 0.42;
            let bw = rect.width() * 0.22;
            p.rect_filled(egui::Rect::from_min_size(pos2(bx, rect.center().y - 4.0), vec2(bw, 8.0)), 3.0, Color32::from_rgb(22, 24, 30));
            p.rect_filled(egui::Rect::from_min_size(pos2(bx, rect.center().y - 4.0), vec2(bw * frac.max(0.01), 8.0)), 3.0, col);
            p.text(pos2(rect.right() - 70.0, rect.center().y), Align2::RIGHT_CENTER, fmt_size(v), FontId::monospace(11.5), Color32::from_gray(220));
            p.text(pos2(rect.right() - 4.0, rect.center().y), Align2::RIGHT_CENTER, fmt_count(e.count), FontId::monospace(11.0), Color32::from_gray(140));
            if resp.clicked() {
                acts.push(Action::HighlightExt(if hl { None } else { Some(k as u16) }));
            }
            resp.on_hover_text(format!("{} files · {:.1}% · double-click to find them all", fmt_count(e.count), frac * 100.0));
        }
    });
}

fn legend_dot(ui: &mut Ui, c: Color32, text: &str) -> egui::Response {
    ui.horizontal(|ui| {
        let (r, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
        ui.painter().rect_filled(r, 2.0, c);
        ui.label(RichText::new(text).size(12.0));
    })
    .response
}

fn cleanup(ui: &mut Ui, t: &Tree, st: &mut State, acts: &mut Vec<Action>) {
    let scope = st.filter_scope.filter(|&s| !t.node(s).is_deleted()).unwrap_or(st.view_root);
    ui.horizontal(|ui| {
        ui.label("In:");
        ui.add(egui::Label::new(RichText::new(t.path(scope)).strong().color(ACCENT)).truncate()).on_hover_text(t.path(scope));
    });
    ui.horizontal(|ui| {
        if let Some(sel) = st.selected
            && t.node(sel).is_dir()
            && ui.small_button("Use selected folder").clicked()
        {
            st.filter_scope = Some(sel);
        }
        if ui.small_button("Use current view").clicked() {
            st.filter_scope = Some(st.view_root);
        }
        if ui.small_button("Whole scan").clicked() {
            st.filter_scope = Some(t.root);
        }
    });
    ui.add_space(4.0);
    ui.label(RichText::new("Quick presets").weak().size(11.0));
    let mut run_now = false;
    ui.horizontal_wrapped(|ui| {
        let f = &mut st.filter;
        let mut preset = |ui: &mut Ui, label: &str, apply: &dyn Fn(&mut FilterSpec)| {
            if ui.small_button(label).clicked() {
                *f = FilterSpec::default();
                apply(f);
                run_now = true;
            }
        };
        preset(ui, "Temp & logs", &|f| f.name = "*.tmp;*.temp;*.log;*.bak;*.old;*.dmp;~*;Thumbs.db".into());
        preset(ui, "Large > 1 GB", &|f| f.min_size = "1GB".into());
        preset(ui, "Not modified in 1 year", &|f| f.modified_before = "1y".into());
        preset(ui, "Videos", &|f| f.exts = "mp4 mkv avi mov wmv m4v webm".into());
        preset(ui, "Installers & images", &|f| f.exts = "msi exe iso img zip 7z rar cab".into());
        preset(ui, "node_modules / build", &|f| {
            f.name = "node_modules;target;.gradle;__pycache__;obj;bin".into();
            f.kind = Kind::Folders;
        });
        preset(ui, "Empty files", &|f| f.max_size = "0".into());
    });
    ui.add_space(6.0);
    let f = &mut st.filter;
    egui::Grid::new("filter_form").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
        ui.label("Name");
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut f.name).hint_text(if f.use_regex { "regular expression" } else { "*.log; cache*; report" }).desired_width(170.0));
            ui.checkbox(&mut f.use_regex, "regex");
            ui.checkbox(&mut f.case_sensitive, "Aa");
        });
        ui.end_row();
        ui.label("Exclude");
        ui.add(egui::TextEdit::singleline(&mut f.exclude).hint_text("*.docx; keep*").desired_width(170.0));
        ui.end_row();
        ui.label("Extensions");
        ui.add(egui::TextEdit::singleline(&mut f.exts).hint_text("mp4 mkv iso").desired_width(170.0));
        ui.end_row();
        ui.label("Size");
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut f.min_size).hint_text("min e.g. 100MB").desired_width(90.0));
            ui.label("–");
            ui.add(egui::TextEdit::singleline(&mut f.max_size).hint_text("max").desired_width(90.0));
        });
        ui.end_row();
        for (label, a, b) in [
            ("Modified", &mut f.modified_after, &mut f.modified_before),
            ("Created", &mut f.created_after, &mut f.created_before),
            ("Accessed", &mut f.accessed_after, &mut f.accessed_before),
        ] {
            ui.label(label);
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(a).hint_text("after").desired_width(90.0)).on_hover_text("YYYY-MM-DD, or relative: 30d, 6m, 2y (= that long ago)");
                ui.label("–");
                ui.add(egui::TextEdit::singleline(b).hint_text("before").desired_width(90.0)).on_hover_text("YYYY-MM-DD, or relative: 30d, 6m, 2y\ne.g. before = 1y → older than a year");
            });
            ui.end_row();
        }
        ui.label("Match");
        ui.horizontal(|ui| {
            ui.selectable_value(&mut f.kind, Kind::Files, "Files");
            ui.selectable_value(&mut f.kind, Kind::Folders, "Folders");
            ui.selectable_value(&mut f.kind, Kind::Both, "Both");
        });
        ui.end_row();
        ui.label("");
        ui.horizontal(|ui| {
            ui.checkbox(&mut f.include_hidden, "hidden");
            ui.checkbox(&mut f.include_system, "system");
        });
        ui.end_row();
    });
    ui.horizontal(|ui| {
        if ui.add(egui::Button::new(RichText::new("🔍 Find matches").strong()).fill(Color32::from_rgb(40, 90, 170))).clicked() {
            run_now = true;
        }
        if ui.button("Reset").clicked() {
            st.filter = FilterSpec::default();
            st.filter_results.clear();
            st.filter_ran = false;
            st.filter_err = None;
        }
    });
    if run_now {
        match st.filter.compile(st.use_alloc) {
            Ok(c) => {
                st.filter_results = filter::run(t, scope, &c, 200_000);
                st.filter_err = None;
                st.filter_ran = true;
            }
            Err(e) => st.filter_err = Some(e),
        }
    }
    if let Some(e) = &st.filter_err {
        ui.colored_label(DANGER, e);
    }
    ui.separator();
    if st.filter_ran {
        let total: u64 = st.filter_results.iter().map(|&i| n_metric(t, i, st.use_alloc)).sum();
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{} matches · {}", fmt_count(st.filter_results.len() as u64), fmt_size(total))).strong());
            if !st.filter_results.is_empty() && ui.button("☑ Select all matches").clicked() {
                acts.push(Action::CheckMany(st.filter_results.clone(), true));
            }
        });
        let ids = std::mem::take(&mut st.filter_results);
        results_table(ui, t, st, &ids, "filter", acts);
        st.filter_results = ids;
    }
}

const RES_ROW: f32 = 34.0;

fn results_table(ui: &mut Ui, t: &Tree, st: &State, ids: &[u32], salt: &str, acts: &mut Vec<Action>) {
    if ids.is_empty() {
        ui.label(RichText::new("No matches.").weak());
        return;
    }
    let all_checked = st.checked.len() >= ids.len() && ids.iter().all(|i| st.checked.contains(i));
    ui.horizontal(|ui| {
        let mut ac = all_checked;
        if ui.checkbox(&mut ac, "all").changed() {
            acts.push(Action::CheckMany(ids.to_vec(), ac));
        }
        if ui.small_button("invert").clicked() {
            let (on, off): (Vec<u32>, Vec<u32>) = ids.iter().partition(|i| !st.checked.contains(i));
            acts.push(Action::CheckMany(off, false));
            acts.push(Action::CheckMany(on, true));
        }
        if ui.small_button("⬇ CSV").on_hover_text("Export this list").clicked()
            && let Some(p) = rfd::FileDialog::new().set_file_name("sizematters.csv").save_file()
        {
            let mut s = String::from("path,size,allocated,modified,created\n");
            for &i in ids {
                let n = t.node(i);
                s += &format!("\"{}\",{},{},{},{}\n", t.path(i).replace('"', "\"\""), n.size, n.alloc, fmt_date(n.modified), fmt_date(n.created));
            }
            let _ = std::fs::write(p, s);
        }
    });
    ui.spacing_mut().item_spacing.y = 0.0;
    egui::ScrollArea::vertical().id_salt(("results", salt)).auto_shrink([false, false]).show_rows(ui, RES_ROW, ids.len(), |ui, range| {
        for ri in range {
            let i = ids[ri];
            let n = t.node(i);
            let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), RES_ROW), Sense::click());
            let p = ui.painter_at(rect);
            let checked = st.checked.contains(&i);
            if st.selected == Some(i) {
                p.rect_filled(rect, 3.0, Color32::from_rgb(40, 74, 130));
            } else if resp.hovered() {
                p.rect_filled(rect, 3.0, Color32::from_rgb(38, 42, 52));
            } else if ri % 2 == 1 {
                p.rect_filled(rect, 0.0, Color32::from_rgb(27, 29, 36));
            }
            // checkbox
            let cb = egui::Rect::from_center_size(pos2(rect.left() + 12.0, rect.center().y), vec2(14.0, 14.0));
            p.rect(cb, 3.0, if checked { ACCENT } else { Color32::from_rgb(30, 33, 40) }, Stroke::new(1.0, Color32::from_gray(120)), StrokeKind::Inside);
            if checked {
                p.text(cb.center(), Align2::CENTER_CENTER, "✔", FontId::proportional(11.0), Color32::WHITE);
            }
            let col = if n.is_dir() { Color32::from_rgb(240, 196, 90) } else { st.ext_colors.get(n.ext as usize).copied().unwrap_or(Color32::GRAY) };
            p.rect_filled(egui::Rect::from_center_size(pos2(rect.left() + 30.0, rect.center().y), vec2(4.0, RES_ROW - 10.0)), 2.0, col);
            let name_w = rect.width() - 150.0;
            let clip = p.with_clip_rect(egui::Rect::from_min_size(rect.min, vec2(name_w + 36.0, RES_ROW)).intersect(rect));
            clip.text(pos2(rect.left() + 38.0, rect.top() + 4.0), Align2::LEFT_TOP, format!("{}{}", if n.is_dir() { "📁 " } else { "" }, t.name(i)), FontId::proportional(13.0), Color32::from_gray(235));
            let parent_path = if n.parent != NONE { t.path(n.parent) } else { String::new() };
            clip.text(pos2(rect.left() + 38.0, rect.bottom() - 4.0), Align2::LEFT_BOTTOM, parent_path, FontId::proportional(10.5), Color32::from_gray(130));
            p.text(pos2(rect.right() - 6.0, rect.top() + 4.0), Align2::RIGHT_TOP, fmt_size(n_metric(t, i, st.use_alloc)), FontId::monospace(12.0), Color32::from_gray(230));
            p.text(pos2(rect.right() - 6.0, rect.bottom() - 4.0), Align2::RIGHT_BOTTOM, fmt_date(n.modified), FontId::proportional(10.5), Color32::from_gray(130));

            if resp.clicked() {
                let on_cb = ui.input(|inp| inp.pointer.interact_pos()).is_some_and(|pp| pp.x < rect.left() + 26.0);
                if on_cb || ui.input(|inp| inp.modifiers.command) {
                    acts.push(Action::Check(i, !checked));
                } else {
                    acts.push(Action::Select(i));
                }
            }
            if resp.double_clicked() {
                acts.push(Action::Check(i, !checked));
            }
            let resp = hover_tip(resp, st.remote, |ui| node_tooltip(ui, t, i));
            resp.context_menu(|ui| node_menu(ui, t, st, i, acts));
        }
    });
}

fn selection_bar(ui: &mut Ui, t: &Tree, st: &mut State, acts: &mut Vec<Action>) {
    let n = st.checked.len();
    if n == 0 {
        ui.label(RichText::new("Tick items in any list (or right-click → Add to delete selection) to delete them in bulk.").weak().size(11.0));
        return;
    }
    // de-dup nested items for an honest total
    let sel: Vec<u32> = st.checked.iter().copied().collect();
    let covered = |i: u32| {
        let mut p = t.node(i).parent;
        while p != NONE {
            if st.checked.contains(&p) {
                return true;
            }
            if p == t.root {
                break;
            }
            p = t.node(p).parent;
        }
        false
    };
    let total: u64 = sel.iter().filter(|&&i| !covered(i)).map(|&i| t.node(i).alloc).sum();
    ui.horizontal(|ui| {
        ui.label(RichText::new(format!("☑ {} selected · {}", fmt_count(n as u64), fmt_size(total))).strong());
        if ui.small_button("clear").clicked() {
            st.checked.clear();
        }
    });
    ui.horizontal(|ui| {
        if t.remote.is_none() && ui.add(egui::Button::new(RichText::new("🗑 Move to Recycle Bin").strong()).fill(Color32::from_rgb(120, 80, 20))).clicked() {
            acts.push(Action::Delete(sel.clone(), Method::RecycleBin));
        }
        if ui.add(egui::Button::new(RichText::new("⚠ Delete permanently").strong()).fill(Color32::from_rgb(130, 35, 35))).clicked() {
            acts.push(Action::Delete(sel, Method::Permanent));
        }
    });
}

// ======================================================================
// SSH connect form & host-key confirmation
// ======================================================================

impl App {
    fn ssh_modals(&mut self, ctx: &egui::Context) {
        let mut connect: Option<(crate::remote::Target, String)> = None;
        let mut close = false;
        if let Some(f) = &mut self.ssh_form {
            let mr = egui::Modal::new(egui::Id::unique("ssh_form")).show(ctx, |ui| {
                ui.set_width(460.0);
                ui.label(RichText::new("🌐 Scan a Linux server over SSH").size(18.0).strong());
                ui.label(RichText::new("Lists the whole folder tree with one remote `find` command (GNU findutils), streamed and compressed.").weak().size(11.5));
                ui.add_space(6.0);
                egui::Grid::new("sshgrid").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                    ui.label("Host");
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut f.host).hint_text("server.example.com").desired_width(230.0));
                        ui.label("port");
                        ui.add(egui::TextEdit::singleline(&mut f.port).desired_width(48.0));
                    });
                    ui.end_row();
                    ui.label("User");
                    ui.add(egui::TextEdit::singleline(&mut f.user).desired_width(150.0));
                    ui.end_row();
                    ui.label("Sign in with");
                    ui.horizontal(|ui| {
                        ui.radio_value(&mut f.use_key, false, "password");
                        ui.radio_value(&mut f.use_key, true, "private key");
                    });
                    ui.end_row();
                    if f.use_key {
                        ui.label("Key file");
                        ui.horizontal(|ui| {
                            ui.add(egui::TextEdit::singleline(&mut f.key_path).desired_width(250.0));
                            if ui.button("📂").clicked()
                                && let Some(p) = rfd::FileDialog::new().pick_file()
                            {
                                f.key_path = p.display().to_string();
                            }
                        });
                        ui.end_row();
                        ui.label("Passphrase");
                        ui.add(egui::TextEdit::singleline(&mut f.key_pass).password(true).hint_text("if the key has one").desired_width(200.0));
                        ui.end_row();
                    } else {
                        ui.label("Password");
                        ui.add(egui::TextEdit::singleline(&mut f.password).password(true).desired_width(200.0));
                        ui.end_row();
                    }
                    ui.label("Folder");
                    ui.add(egui::TextEdit::singleline(&mut f.path).desired_width(250.0));
                    ui.end_row();
                });
                if let Some(e) = &f.error {
                    ui.colored_label(DANGER, e);
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        if ui.add(egui::Button::new(RichText::new("Connect & scan").strong()).fill(Color32::from_rgb(40, 90, 170))).clicked()
                            || ui.input(|i| i.key_pressed(Key::Enter))
                        {
                            match f.target() {
                                Ok(t) => connect = Some((t, f.path.trim().to_string())),
                                Err(e) => f.error = Some(e),
                            }
                        }
                    });
                });
            });
            if mr.should_close() {
                close = true;
            }
        }
        if let Some((target, path)) = connect {
            let mut remembered = self.ssh_form.take().unwrap_or_default();
            remembered.error = None;
            self.last_ssh = Some(remembered);
            self.start_request(ScanRequest { path: PathBuf::from(if path.is_empty() { "/".into() } else { path }), prefer_mft: false, remote: Some(target), conn: None }, ctx);
        } else if close {
            self.ssh_form = None;
        }

        // unknown host: confirm fingerprint
        let mut trust: Option<bool> = None;
        if let Some((t, fp, _)) = &self.trust_prompt {
            egui::Modal::new(egui::Id::unique("ssh_trust")).show(ctx, |ui| {
                ui.set_width(480.0);
                ui.label(RichText::new("New SSH host").size(18.0).strong());
                ui.label(format!("{} has not been seen before. Its host key fingerprint is:", t.host));
                ui.add_space(4.0);
                ui.label(RichText::new(fp).monospace().size(13.0).color(WARN));
                ui.add_space(4.0);
                ui.label(RichText::new("Only continue if this matches the server's real key (ask its administrator, or run `ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub` on it). It will be saved to known_hosts.").weak().size(11.5));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        trust = Some(false);
                    }
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        if ui.add(egui::Button::new(RichText::new("Trust & connect").strong()).fill(Color32::from_rgb(40, 90, 170))).clicked() {
                            trust = Some(true);
                        }
                    });
                });
            });
        }
        match trust {
            Some(true) => {
                let (mut t, fp, path) = self.trust_prompt.take().unwrap();
                t.trust_fingerprint = Some(fp);
                self.start_request(ScanRequest { path, prefer_mft: false, remote: Some(t), conn: None }, ctx);
            }
            Some(false) => self.trust_prompt = None,
            None => {}
        }
    }
}

// ======================================================================
// Actions & modals
// ======================================================================

impl App {
    fn apply_actions(&mut self, acts: Vec<Action>, ctx: &egui::Context) {
        let Some(t) = &self.tree else { return };
        let st = &mut self.st;
        for a in acts {
            match a {
                Action::TreeSelect(i) => {
                    st.selected = Some(i);
                    // the chart follows: show the folder itself, or the folder holding the file
                    let target = if t.node(i).is_dir() { i } else { t.node(i).parent };
                    if target != NONE {
                        st.view_root = target;
                    }
                    st.scroll_to_selected = true;
                }
                Action::Select(i) => {
                    st.selected = Some(i);
                    {
                        // reveal in tree
                        let chain = t.ancestry(i);
                        let mut changed = false;
                        for &c in &chain[..chain.len().saturating_sub(1)] {
                            changed |= st.expanded.insert(c);
                        }
                        if changed {
                            st.rows_dirty = true;
                            rebuild_rows(t, st);
                        }
                        st.scroll_to_selected = true;
                    }
                }
                Action::Zoom(i) => {
                    if t.node(i).is_dir() {
                        st.view_root = i;
                        for &c in &t.ancestry(i) {
                            st.expanded.insert(c);
                        }
                        st.rows_dirty = true;
                    }
                }
                Action::ZoomOut => {
                    if st.view_root != t.root {
                        let prev = st.view_root;
                        st.view_root = t.node(prev).parent;
                        st.selected = Some(prev);
                    }
                }
                Action::Toggle(i) => {
                    if !st.expanded.remove(&i) {
                        st.expanded.insert(i);
                    }
                    st.rows_dirty = true;
                }
                Action::Delete(_, _) if st.viewing_saved.is_some() => {
                    st.notice = Some(("This is a saved scan (read-only). Rescan the location to delete files.".into(), WARN));
                }
                Action::Delete(ids, m) => {
                    let m = if t.remote.is_some() { Method::Permanent } else { m };
                    let plan = delete::plan(t, &ids);
                    st.confirm = Some(Confirm { plan, method: m, typed: String::new(), ack: false });
                }
                Action::Reveal(i) => platform::reveal(&PathBuf::from(t.path(i))),
                Action::Open(i) => platform::open(&PathBuf::from(t.path(i))),
                Action::CopyPath(i) => ctx.copy_text(t.path(i)),
                Action::Scope(i) => {
                    st.filter_scope = Some(i);
                    st.tab = Tab::Cleanup;
                }
                Action::Check(i, on) => {
                    if on {
                        st.checked.insert(i);
                    } else {
                        st.checked.remove(&i);
                    }
                }
                Action::CheckMany(v, on) => {
                    for i in v {
                        if on {
                            st.checked.insert(i);
                        } else {
                            st.checked.remove(&i);
                        }
                    }
                }
                Action::HighlightExt(e) => st.highlight_ext = e,
            }
        }
    }

    fn modals(&mut self, ctx: &egui::Context) {
        // ---- confirmation ----
        let mut start: Option<(Vec<delete::Item>, Method)> = None;
        if let Some(c) = &mut self.st.confirm {
            let mut close = false;
            egui::Modal::new(egui::Id::unique("confirm")).show(ctx, |ui| {
                ui.set_width(560.0);
                let p = &c.plan;
                let perm = c.method == Method::Permanent;
                ui.label(RichText::new(if perm { "⚠ Permanently delete?" } else { "🗑 Move to Recycle Bin?" }).size(20.0).strong().color(if perm { DANGER } else { WARN }));
                ui.add_space(6.0);
                if p.items.is_empty() {
                    ui.label("Nothing that can be deleted is selected.");
                } else {
                    ui.label(RichText::new(format!(
                        "{} item(s) · {} files inside · frees about {}",
                        fmt_count(p.items.len() as u64),
                        fmt_count(p.total_files),
                        fmt_size(p.total_alloc)
                    ))
                    .size(15.0));
                }
                ui.add_space(6.0);
                egui::ScrollArea::vertical().max_height(200.0).show(ui, |ui| {
                    for it in p.items.iter().take(500) {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(fmt_size(it.alloc)).monospace().color(Color32::from_gray(200)));
                            ui.add(egui::Label::new(format!("{}{}", if it.is_dir { "📁 " } else { "" }, it.path.display())).truncate()).on_hover_text(it.path.display().to_string());
                        });
                    }
                    if p.items.len() > 500 {
                        ui.label(RichText::new(format!("… and {} more", p.items.len() - 500)).weak());
                    }
                });
                if !p.blocked.is_empty() {
                    ui.add_space(6.0);
                    ui.label(RichText::new(format!("🛡 {} protected item(s) will be skipped:", p.blocked.len())).color(WARN));
                    egui::ScrollArea::vertical().id_salt("blk").max_height(80.0).show(ui, |ui| {
                        for (path, why) in p.blocked.iter().take(100) {
                            ui.add(egui::Label::new(RichText::new(format!("{} — {why}", path.display())).size(11.5).weak()).truncate());
                        }
                    });
                }
                if p.has_cloud {
                    ui.colored_label(WARN, "☁ Some items are cloud placeholders: deleting them frees little space locally and also deletes them from the cloud.");
                }
                if p.has_hidden_system {
                    ui.colored_label(WARN, "Some items are hidden system files.");
                }
                ui.add_space(8.0);
                if let Some(conn) = self.tree.as_ref().and_then(|t| t.remote.as_ref()) {
                    ui.label(RichText::new(format!("🌐 Deleting on {} — remote deletes are permanent (rm -rf, links are not followed).", conn.target.label())).color(WARN));
                } else {
                    ui.horizontal(|ui| {
                        ui.radio_value(&mut c.method, Method::RecycleBin, "Recycle Bin (undoable)");
                        ui.radio_value(&mut c.method, Method::Permanent, "Permanent");
                    });
                }
                let perm = c.method == Method::Permanent;
                let big = p.total_alloc > 1 << 30 || p.items.len() > 50;
                let mut ok_to_go = !p.items.is_empty();
                if perm {
                    ui.checkbox(&mut c.ack, "I understand this cannot be undone");
                    ok_to_go &= c.ack;
                    if big {
                        ui.horizontal(|ui| {
                            ui.label("Type DELETE to confirm:");
                            ui.add(egui::TextEdit::singleline(&mut c.typed).desired_width(100.0));
                        });
                        ok_to_go &= c.typed.trim() == "DELETE";
                    }
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                        close = true;
                    }
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        let (txt, fill) = if perm { ("Delete permanently", Color32::from_rgb(160, 40, 40)) } else { ("Move to Recycle Bin", Color32::from_rgb(140, 95, 25)) };
                        if ui.add_enabled(ok_to_go, egui::Button::new(RichText::new(txt).strong()).fill(fill)).clicked() {
                            start = Some((p.items.clone(), c.method));
                            close = true;
                        }
                    });
                });
            });
            if close {
                self.st.confirm = None;
            }
        }
        if let Some((items, m)) = start {
            let remote = self.tree.as_ref().and_then(|t| t.remote.clone());
            let job = delete::execute(items.clone(), m, ctx.clone(), remote);
            self.st.job = Some((job, items, m));
        }

        // ---- progress ----
        if let Some((job, _, _)) = &self.st.job {
            egui::Modal::new(egui::Id::unique("progress")).show(ctx, |ui| {
                ui.set_width(420.0);
                ui.label(RichText::new("Deleting…").size(18.0).strong());
                let d = job.done.load(Ordering::Relaxed);
                ui.add(egui::ProgressBar::new(d as f32 / job.total.max(1) as f32).text(format!("{d} / {}", job.total)));
                if ui.button("Stop").clicked() {
                    job.cancel.store(true, Ordering::Relaxed);
                }
            });
        }

        // ---- report ----
        if let Some(r) = &self.st.report {
            let mut close = false;
            egui::Modal::new(egui::Id::unique("report")).show(ctx, |ui| {
                ui.set_width(520.0);
                ui.label(RichText::new(format!("✔ Removed {} item(s), {} freed", r.ok, fmt_size(r.freed))).size(18.0).strong().color(OK));
                if r.method == Method::RecycleBin && r.ok > 0 {
                    ui.label(RichText::new("Space is released once the Recycle Bin is emptied.").weak());
                }
                if !r.failures.is_empty() {
                    ui.add_space(6.0);
                    ui.label(RichText::new(format!("{} item(s) could not be removed:", r.failures.len())).color(DANGER));
                    egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                        for (p, e) in &r.failures {
                            ui.label(RichText::new(format!("{p}\n    {e}")).size(11.5));
                        }
                    });
                }
                ui.add_space(8.0);
                if ui.button("OK").clicked() || ui.input(|i| i.key_pressed(Key::Enter) || i.key_pressed(Key::Escape)) {
                    close = true;
                }
            });
            if close {
                self.st.report = None;
            }
        }
        let _ = Pos2::ZERO;
    }
}
