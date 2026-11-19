//! Folder compare screen ("thin WinMerge"): a merged tree of two folders with
//! New / Deleted / Edited markers, filters, side-by-side sizes & dates, copy in either
//! direction, delete, and a line diff for text files.

use super::theme::{ACCENT, DANGER, OK, WARN};
use crate::compare::{self, CopyJob, DNode, Diff, Line, Status};
use crate::util::{fmt_count, fmt_date, fmt_size};
use egui::{Align, Align2, Color32, FontId, RichText, Sense, Stroke, StrokeKind, Ui, pos2, vec2};
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

const ROW: f32 = 22.0;

pub struct CompareState {
    left: String,
    right: String,
    content: bool,
    show: [bool; 4], // new, deleted, edited, same
    name_filter: String,
    rx: Option<crossbeam_channel::Receiver<Diff>>,
    progress: Arc<compare::Progress>,
    started: Instant,
    diff: Option<Diff>,
    expanded: HashSet<String>,
    rows: Vec<(usize, u16)>,
    rows_dirty: bool,
    checked: HashSet<usize>,
    selected: Option<usize>,
    confirm: Option<Pending>,
    copy: Option<(Arc<CopyJob>, bool)>,
    report: Option<String>,
    text: Option<(String, Result<Vec<Line>, String>)>,
    refresh: bool,
}

enum Pending {
    Copy { rels: Vec<String>, to_right: bool, bytes: u64, overwrites: usize },
    Delete { rels: Vec<String>, right_side: bool, bytes: u64 },
}

impl Default for CompareState {
    fn default() -> Self {
        CompareState {
            left: String::new(),
            right: String::new(),
            content: false,
            show: [true, true, true, false],
            name_filter: String::new(),
            rx: None,
            progress: Arc::new(compare::Progress::default()),
            started: Instant::now(),
            diff: None,
            expanded: HashSet::new(),
            rows: Vec::new(),
            rows_dirty: true,
            checked: HashSet::new(),
            selected: None,
            confirm: None,
            copy: None,
            report: None,
            text: None,
            refresh: false,
        }
    }
}

fn status_color(s: Status) -> Color32 {
    match s {
        Status::New => OK,
        Status::Deleted => DANGER,
        Status::Edited => WARN,
        Status::Same => Color32::from_gray(150),
    }
}

fn status_label(s: Status) -> &'static str {
    match s {
        Status::New => "new",
        Status::Deleted => "deleted",
        Status::Edited => "edited",
        Status::Same => "same",
    }
}

impl CompareState {
    pub fn preset(&mut self, left: &str, right: &str, ctx: &egui::Context) {
        self.left = left.to_string();
        self.right = right.to_string();
        self.start(ctx);
    }

    fn start(&mut self, ctx: &egui::Context) {
        let (l, r) = (std::path::PathBuf::from(self.left.trim()), std::path::PathBuf::from(self.right.trim()));
        if !l.is_dir() || !r.is_dir() {
            self.report = Some("Pick two existing folders to compare.".into());
            return;
        }
        let (tx, rx) = crossbeam_channel::bounded(1);
        self.progress = Arc::new(compare::Progress::default());
        let p = self.progress.clone();
        let content = self.content;
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let d = compare::compare(&l, &r, content, &p);
            let _ = tx.send(d);
            ctx.request_repaint();
        });
        self.rx = Some(rx);
        self.started = Instant::now();
    }

    fn poll(&mut self) {
        if let Some(rx) = &self.rx
            && let Ok(d) = rx.try_recv()
        {
            self.rx = None;
            if self.diff.is_none() {
                self.expanded.clear();
                self.expanded.insert(String::new());
                // open the first level of changed folders
                for &c in &d.nodes[0].children {
                    if d.nodes[c].is_dir() && d.nodes[c].changed() {
                        self.expanded.insert(d.nodes[c].rel.clone());
                    }
                }
            }
            self.checked.clear();
            self.selected = None;
            self.diff = Some(d);
            self.rows_dirty = true;
        }
        if let Some((job, _)) = &self.copy
            && job.finished.load(Ordering::SeqCst)
        {
            let errs = job.errors.lock().clone();
            let done = job.done_items.load(Ordering::Relaxed);
            self.report = Some(if errs.is_empty() {
                format!("Copied {done} item(s), {}.", fmt_size(job.done_bytes.load(Ordering::Relaxed)))
            } else {
                format!(
                    "Copied {done} item(s); {} error(s):\n{}",
                    errs.len(),
                    errs.iter().take(20).map(|(p, e)| format!("{p}: {e}")).collect::<Vec<_>>().join("\n")
                )
            });
            self.copy = None;
            self.refresh = true; // re-compare so copied items show as identical
        }
    }

    fn visible(&self, d: &Diff, i: usize) -> bool {
        let n = &d.nodes[i];
        if !self.name_filter.trim().is_empty() && !n.is_dir() && !n.name.to_lowercase().contains(&self.name_filter.trim().to_lowercase()) {
            return false;
        }
        if n.is_dir() {
            // a folder is shown if it is itself new/deleted, or contains anything shown
            return match n.status {
                Status::New => self.show[0],
                Status::Deleted => self.show[1],
                Status::Edited => self.show[0] && n.counts[0] > 0 || self.show[1] && n.counts[1] > 0 || self.show[2] && n.counts[2] > 0 || self.show[3],
                Status::Same => self.show[3],
            } || !self.name_filter.trim().is_empty() && n.children.iter().any(|&c| self.visible(d, c));
        }
        match n.status {
            Status::New => self.show[0],
            Status::Deleted => self.show[1],
            Status::Edited => self.show[2],
            Status::Same => self.show[3],
        }
    }

    fn rebuild_rows(&mut self) {
        self.rows.clear();
        let Some(d) = &self.diff else { return };
        let mut stack: Vec<(usize, u16)> = d.nodes[0].children.iter().rev().map(|&c| (c, 0)).collect();
        while let Some((i, depth)) = stack.pop() {
            if !self.visible(d, i) {
                continue;
            }
            self.rows.push((i, depth));
            if d.nodes[i].is_dir() && self.expanded.contains(&d.nodes[i].rel) {
                for &c in d.nodes[i].children.iter().rev() {
                    stack.push((c, depth + 1));
                }
            }
        }
        self.rows_dirty = false;
    }

    /// Checked items, without children of checked folders.
    fn top_checked(&self, d: &Diff) -> Vec<usize> {
        let mut v: Vec<usize> = self
            .checked
            .iter()
            .copied()
            .filter(|&i| {
                let mut p = d.nodes[i].parent;
                while p != usize::MAX && p != 0 {
                    if self.checked.contains(&p) {
                        return false;
                    }
                    p = d.nodes[p].parent;
                }
                true
            })
            .collect();
        v.sort_unstable();
        v
    }
}

pub fn ui(ui: &mut Ui, cs: &mut CompareState) {
    let ctx = ui.ctx().clone();
    cs.poll();
    if cs.refresh {
        cs.refresh = false;
        cs.start(&ctx);
    }
    if cs.rx.is_some() || cs.copy.is_some() {
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
    }

    // ---- folder pickers ----
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        let w = ((ui.available_width() - 330.0) / 2.0).max(160.0);
        for (label, path, col) in [("Left (original)", &mut cs.left, DANGER), ("Right (new)", &mut cs.right, OK)] {
            ui.label(RichText::new(label).color(col).strong());
            ui.add(egui::TextEdit::singleline(path).desired_width(w - 130.0).hint_text("folder path"));
            if ui.button("📂").on_hover_text("Browse…").clicked()
                && let Some(p) = rfd::FileDialog::new().pick_folder()
            {
                *path = p.display().to_string();
            }
            ui.add_space(6.0);
        }
    });
    ui.horizontal(|ui| {
        if ui.button("⇄ Swap").clicked() {
            std::mem::swap(&mut cs.left, &mut cs.right);
            if cs.diff.is_some() {
                cs.diff = None;
                cs.start(&ctx);
            }
        }
        ui.checkbox(&mut cs.content, "Compare file contents").on_hover_text("Hash files whose size matches (slower, exact). Otherwise size + modified date are compared.");
        let busy = cs.rx.is_some();
        if ui.add_enabled(!busy, egui::Button::new(RichText::new("⇄ Compare").strong()).fill(Color32::from_rgb(40, 90, 170))).clicked() {
            cs.diff = None;
            cs.start(&ctx);
        }
        if busy {
            ui.spinner();
            let p = &cs.progress;
            let (h, th) = (p.hashed.load(Ordering::Relaxed), p.to_hash.load(Ordering::Relaxed));
            ui.label(if th > 0 {
                format!("hashing {h} / {th}")
            } else {
                format!("{} items listed · {:.1}s", fmt_count(p.listed.load(Ordering::Relaxed)), cs.started.elapsed().as_secs_f32())
            });
            if ui.button("Cancel").clicked() {
                p.cancel.store(true, Ordering::Relaxed);
            }
        }
    });
    ui.separator();

    let Some(d) = &cs.diff else {
        ui.add_space(40.0);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new("Compare two folders").size(24.0).strong());
            ui.label(RichText::new("See what is new, edited or deleted between an original and a newer copy, then copy items across in either direction.").weak());
        });
        modals(&ctx, cs);
        return;
    };

    // ---- summary + filters ----
    let root = &d.nodes[0];
    let mut refilter = false;
    ui.horizontal_wrapped(|ui| {
        for (k, (lbl, col)) in [("new", OK), ("deleted", DANGER), ("edited", WARN)].into_iter().enumerate() {
            let txt = RichText::new(format!("{} {} {}  ({})", ["＋", "−", "~"][k], fmt_count(root.counts[k] as u64), lbl, fmt_size(root.bytes[k]))).color(if cs.show[k] { col } else { Color32::from_gray(110) });
            refilter |= ui.toggle_value(&mut cs.show[k], txt).changed();
        }
        refilter |= ui.toggle_value(&mut cs.show[3], RichText::new("= identical")).changed();
        ui.separator();
        refilter |= ui.add(egui::TextEdit::singleline(&mut cs.name_filter).hint_text("filter names").desired_width(140.0)).changed();
        ui.separator();
        if ui.small_button("expand all").clicked() {
            for n in &d.nodes {
                if n.is_dir() && n.changed() {
                    cs.expanded.insert(n.rel.clone());
                }
            }
            refilter = true;
        }
        if ui.small_button("collapse").clicked() {
            cs.expanded.clear();
            refilter = true;
        }
        ui.label(RichText::new(format!("compared in {:.2}s{}", d.elapsed_ms as f64 / 1000.0, if d.content_compared { " · by content" } else { " · by size & date" })).weak().size(11.0));
    });
    if refilter {
        cs.rows_dirty = true;
    }
    if cs.rows_dirty {
        cs.rebuild_rows();
    }
    // Take the diff out while drawing so selection state can be mutated freely.
    let d_owned = cs.diff.take().unwrap();
    let d = &d_owned;

    // ---- action bar (bottom) ----
    let mut action: Option<Pending> = None;
    egui::Panel::bottom("cmp_actions").frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(0, 6))).show(ui, |ui| {
        let top = cs.top_checked(d);
        let size_l: u64 = top.iter().map(|&i| d.nodes[i].left.map(|m| m.size).unwrap_or(0)).sum();
        let size_r: u64 = top.iter().map(|&i| d.nodes[i].right.map(|m| m.size).unwrap_or(0)).sum();
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("☑ {} selected", top.len())).strong());
            ui.menu_button("Select…", |ui| {
                for (lbl, st) in [("all new", Status::New), ("all deleted", Status::Deleted), ("all edited", Status::Edited)] {
                    if ui.button(lbl).clicked() {
                        for (i, n) in d.nodes.iter().enumerate() {
                            if i > 0 && n.status == st && (!n.is_dir() || st != Status::Edited) {
                                cs.checked.insert(i);
                            }
                        }
                        ui.close();
                    }
                }
                if ui.button("all differences").clicked() {
                    for (i, n) in d.nodes.iter().enumerate() {
                        if i > 0 && n.changed() && !(n.is_dir() && n.status == Status::Edited) {
                            cs.checked.insert(i);
                        }
                    }
                    ui.close();
                }
                if ui.button("none").clicked() {
                    cs.checked.clear();
                    ui.close();
                }
            });
            ui.separator();
            let can_r = top.iter().any(|&i| d.nodes[i].left.is_some());
            let can_l = top.iter().any(|&i| d.nodes[i].right.is_some());
            if ui.add_enabled(can_r, egui::Button::new(RichText::new(format!("Copy → Right  ({})", fmt_size(size_l))).strong()).fill(Color32::from_rgb(35, 95, 70))).on_hover_text("Copy the left version of the selected items to the right folder (overwrites)").clicked() {
                let rels: Vec<String> = top.iter().filter(|&&i| d.nodes[i].left.is_some()).map(|&i| d.nodes[i].rel.clone()).collect();
                let overwrites = top.iter().filter(|&&i| d.nodes[i].left.is_some() && d.nodes[i].right.is_some()).count();
                action = Some(Pending::Copy { rels, to_right: true, bytes: size_l, overwrites });
            }
            if ui.add_enabled(can_l, egui::Button::new(RichText::new(format!("Copy ← Left  ({})", fmt_size(size_r))).strong()).fill(Color32::from_rgb(35, 70, 110))).on_hover_text("Copy the right version of the selected items to the left folder (overwrites)").clicked() {
                let rels: Vec<String> = top.iter().filter(|&&i| d.nodes[i].right.is_some()).map(|&i| d.nodes[i].rel.clone()).collect();
                let overwrites = top.iter().filter(|&&i| d.nodes[i].left.is_some() && d.nodes[i].right.is_some()).count();
                action = Some(Pending::Copy { rels, to_right: false, bytes: size_r, overwrites });
            }
            ui.separator();
            if ui.add_enabled(can_l, egui::Button::new(RichText::new("🗑 Delete on right").color(DANGER))).clicked() {
                let rels = top.iter().filter(|&&i| d.nodes[i].right.is_some()).map(|&i| d.nodes[i].rel.clone()).collect();
                action = Some(Pending::Delete { rels, right_side: true, bytes: size_r });
            }
            if ui.add_enabled(can_r, egui::Button::new(RichText::new("🗑 Delete on left").color(DANGER))).clicked() {
                let rels = top.iter().filter(|&&i| d.nodes[i].left.is_some()).map(|&i| d.nodes[i].rel.clone()).collect();
                action = Some(Pending::Delete { rels, right_side: false, bytes: size_l });
            }
            if let Some((job, to_right)) = &cs.copy {
                ui.separator();
                let (a, b) = (job.done_bytes.load(Ordering::Relaxed), job.total_bytes.load(Ordering::Relaxed).max(1));
                ui.add(egui::ProgressBar::new(a as f32 / b as f32).desired_width(180.0).text(format!("copying {} {}", if *to_right { "→" } else { "←" }, fmt_size(a))));
                if ui.small_button("Stop").clicked() {
                    job.cancel.store(true, Ordering::Relaxed);
                }
            }
        });
    });
    if action.is_some() {
        cs.confirm = action;
    }

    // ---- table ----
    let full_w = ui.available_width();
    let col_size = 80.0;
    let col_date = 120.0;
    let right_block = 2.0 * (col_size + col_date) + 70.0;
    let name_w = (full_w - right_block - 30.0).max(160.0);
    let x_ls = 28.0 + name_w;
    let x_ld = x_ls + col_size;
    let x_st = x_ld + col_date;
    let x_rs = x_st + 70.0;
    let x_rd = x_rs + col_size;
    let (hr, _) = ui.allocate_exact_size(vec2(full_w, 20.0), Sense::hover());
    {
        let p = ui.painter();
        let f = FontId::proportional(12.0);
        let c = Color32::from_gray(150);
        let y = hr.center().y;
        p.text(pos2(hr.left() + 28.0, y), Align2::LEFT_CENTER, "Name", f.clone(), c);
        p.text(pos2(hr.left() + x_ls + col_size - 6.0, y), Align2::RIGHT_CENTER, "Left size", f.clone(), DANGER.gamma_multiply(0.8));
        p.text(pos2(hr.left() + x_ld + 6.0, y), Align2::LEFT_CENTER, "Left modified", f.clone(), DANGER.gamma_multiply(0.8));
        p.text(pos2(hr.left() + x_st + 35.0, y), Align2::CENTER_CENTER, "Status", f.clone(), c);
        p.text(pos2(hr.left() + x_rs + col_size - 6.0, y), Align2::RIGHT_CENTER, "Right size", f.clone(), OK.gamma_multiply(0.8));
        p.text(pos2(hr.left() + x_rd + 6.0, y), Align2::LEFT_CENTER, "Right modified", f, OK.gamma_multiply(0.8));
    }
    let mut toggle: Option<usize> = None;
    let mut open_text: Option<usize> = None;
    ui.spacing_mut().item_spacing.y = 0.0;
    let rows = std::mem::take(&mut cs.rows);
    egui::ScrollArea::vertical().id_salt("cmp_rows").auto_shrink([false, false]).show_rows(ui, ROW, rows.len(), |ui, range| {
        for ri in range {
            let (i, depth) = rows[ri];
            let n: &DNode = &d.nodes[i];
            let (rect, resp) = ui.allocate_exact_size(vec2(full_w, ROW), Sense::click());
            let p = ui.painter_at(rect);
            let col = status_color(n.status);
            if cs.selected == Some(i) {
                p.rect_filled(rect, 3.0, Color32::from_rgb(40, 74, 130));
            } else if resp.hovered() {
                p.rect_filled(rect, 3.0, Color32::from_rgb(38, 42, 52));
            } else if n.status != Status::Same && !n.is_dir() {
                p.rect_filled(rect, 0.0, col.gamma_multiply(0.10));
            } else if ri % 2 == 1 {
                p.rect_filled(rect, 0.0, Color32::from_rgb(27, 29, 36));
            }
            let cy = rect.center().y;
            // checkbox
            let checked = cs.checked.contains(&i);
            let cb = egui::Rect::from_center_size(pos2(rect.left() + 12.0, cy), vec2(13.0, 13.0));
            p.rect(cb, 3.0, if checked { ACCENT } else { Color32::from_rgb(30, 33, 40) }, Stroke::new(1.0, Color32::from_gray(120)), StrokeKind::Inside);
            if checked {
                p.text(cb.center(), Align2::CENTER_CENTER, "✔", FontId::proportional(10.0), Color32::WHITE);
            }
            // name with tree indent
            let x = rect.left() + 28.0 + depth as f32 * 14.0;
            if n.is_dir() {
                super::draw_arrow(&p, pos2(x + 5.0, cy), cs.expanded.contains(&n.rel), Color32::from_gray(170));
            }
            super::draw_icon(&p, pos2(x + 20.0, cy), n.is_dir(), false, if n.is_dir() { Color32::from_rgb(240, 196, 90) } else { col });
            let name_col = match n.status {
                Status::Same => Color32::from_gray(200),
                s => status_color(s),
            };
            let clip = egui::Rect::from_min_max(pos2(x + 28.0, rect.top()), pos2(rect.left() + x_ls - 4.0, rect.bottom()));
            let mut label = n.name.clone();
            if n.is_dir() && n.status == Status::Edited {
                let c = n.counts;
                label += &format!("   +{} −{} ~{}", c[0], c[1], c[2]);
            }
            p.with_clip_rect(clip).text(pos2(x + 30.0, cy), Align2::LEFT_CENTER, label, FontId::proportional(13.0), name_col);
            // left / right columns
            let mono = FontId::monospace(11.5);
            if let Some(m) = n.left {
                p.text(pos2(rect.left() + x_ls + col_size - 6.0, cy), Align2::RIGHT_CENTER, fmt_size(m.size), mono.clone(), Color32::from_gray(210));
                if !m.is_dir {
                    p.text(pos2(rect.left() + x_ld + 6.0, cy), Align2::LEFT_CENTER, fmt_date(m.mtime), FontId::proportional(11.5), Color32::from_gray(150));
                }
            }
            let badge = egui::Rect::from_center_size(pos2(rect.left() + x_st + 35.0, cy), vec2(60.0, 16.0));
            if n.status != Status::Same {
                p.rect_filled(badge, 8.0, col.gamma_multiply(0.25));
                p.text(badge.center(), Align2::CENTER_CENTER, if n.touched_only { "touched" } else { status_label(n.status) }, FontId::proportional(10.5), col);
            } else {
                p.text(badge.center(), Align2::CENTER_CENTER, if n.touched_only { "date only" } else { "=" }, FontId::proportional(11.0), Color32::from_gray(110));
            }
            if let Some(m) = n.right {
                p.text(pos2(rect.left() + x_rs + col_size - 6.0, cy), Align2::RIGHT_CENTER, fmt_size(m.size), mono, Color32::from_gray(210));
                if !m.is_dir {
                    p.text(pos2(rect.left() + x_rd + 6.0, cy), Align2::LEFT_CENTER, fmt_date(m.mtime), FontId::proportional(11.5), Color32::from_gray(150));
                }
            }
            if resp.clicked() {
                let px = ui.input(|inp| inp.pointer.interact_pos()).map(|pp| pp.x - rect.left()).unwrap_or(999.0);
                if px < 24.0 || ui.input(|inp| inp.modifiers.command) {
                    if checked {
                        cs.checked.remove(&i);
                    } else {
                        cs.checked.insert(i);
                    }
                } else {
                    cs.selected = Some(i);
                    if n.is_dir() && px < 28.0 + depth as f32 * 14.0 + 14.0 {
                        toggle = Some(i);
                    }
                }
            }
            if resp.double_clicked() {
                if n.is_dir() {
                    toggle = Some(i);
                } else {
                    open_text = Some(i);
                }
            }
            resp.context_menu(|ui| {
                ui.label(RichText::new(&n.name).strong());
                ui.separator();
                if !n.is_dir() && ui.button("🔍 Show text differences").clicked() {
                    open_text = Some(i);
                    ui.close();
                }
                if n.left.is_some() && ui.button("📂 Show left in Explorer").clicked() {
                    crate::platform::reveal(&d.abs(false, &n.rel));
                    ui.close();
                }
                if n.right.is_some() && ui.button("📂 Show right in Explorer").clicked() {
                    crate::platform::reveal(&d.abs(true, &n.rel));
                    ui.close();
                }
                if ui.button(if checked { "☐ Unselect" } else { "☑ Select" }).clicked() {
                    if checked {
                        cs.checked.remove(&i);
                    } else {
                        cs.checked.insert(i);
                    }
                    ui.close();
                }
            });
        }
    });
    cs.rows = rows;
    if let Some(i) = toggle {
        let rel = d.nodes[i].rel.clone();
        if !cs.expanded.remove(&rel) {
            cs.expanded.insert(rel);
        }
        cs.rows_dirty = true;
    }
    if let Some(i) = open_text {
        let n = &d.nodes[i];
        let lp = n.left.map(|_| d.abs(false, &n.rel));
        let rp = n.right.map(|_| d.abs(true, &n.rel));
        cs.text = Some((n.rel.clone(), compare::text_diff(lp.as_deref(), rp.as_deref())));
    }
    cs.diff = Some(d_owned);
    modals(&ctx, cs);
}

fn modals(ctx: &egui::Context, cs: &mut CompareState) {
    // confirm copy / delete
    let mut close = false;
    let mut go: Option<Pending> = None;
    if let Some(p) = &cs.confirm {
        let mr = egui::Modal::new(egui::Id::unique("cmp_confirm")).show(ctx, |ui| {
            ui.set_width(520.0);
            match p {
                Pending::Copy { rels, to_right, bytes, overwrites } => {
                    ui.label(RichText::new(format!("Copy {} item(s) {}?", rels.len(), if *to_right { "→ right" } else { "← left" })).size(18.0).strong());
                    ui.label(format!("{} · {} existing item(s) will be overwritten", fmt_size(*bytes), overwrites));
                }
                Pending::Delete { rels, right_side, bytes } => {
                    ui.label(RichText::new(format!("Move {} item(s) on the {} to the Recycle Bin?", rels.len(), if *right_side { "right" } else { "left" })).size(18.0).strong().color(DANGER));
                    ui.label(fmt_size(*bytes));
                }
            }
            let rels = match p {
                Pending::Copy { rels, .. } | Pending::Delete { rels, .. } => rels,
            };
            egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                for r in rels.iter().take(300) {
                    ui.add(egui::Label::new(RichText::new(r).size(12.0)).truncate());
                }
                if rels.len() > 300 {
                    ui.label(format!("… and {} more", rels.len() - 300));
                }
            });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    close = true;
                }
                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    if ui.add(egui::Button::new(RichText::new("Go").strong()).fill(Color32::from_rgb(40, 90, 170))).clicked() {
                        close = true;
                        go = Some(match p {
                            Pending::Copy { rels, to_right, bytes, overwrites } => Pending::Copy { rels: rels.clone(), to_right: *to_right, bytes: *bytes, overwrites: *overwrites },
                            Pending::Delete { rels, right_side, bytes } => Pending::Delete { rels: rels.clone(), right_side: *right_side, bytes: *bytes },
                        });
                    }
                });
            });
        });
        if mr.should_close() {
            close = true;
        }
    }
    if close {
        cs.confirm = None;
    }
    if let (Some(g), Some(d)) = (go, &cs.diff) {
        match g {
            Pending::Copy { rels, to_right, .. } => {
                let job = Arc::new(CopyJob::default());
                compare::copy_items(d.left.clone(), d.right.clone(), rels, to_right, job.clone(), ctx.clone());
                cs.copy = Some((job, to_right));
            }
            Pending::Delete { rels, right_side, .. } => {
                let paths: Vec<std::path::PathBuf> = rels.iter().map(|r| d.abs(right_side, r)).collect();
                let res = trash::delete_all(&paths);
                cs.report = Some(match res {
                    Ok(()) => format!("Moved {} item(s) to the Recycle Bin.", paths.len()),
                    Err(e) => format!("Delete failed: {e}"),
                });
                cs.start(ctx); // refresh
            }
        }
    }
    // text diff window
    let mut close_text = false;
    if let Some((rel, res)) = &cs.text {
        let mr = egui::Modal::new(egui::Id::unique("cmp_text")).show(ctx, |ui| {
            let sz = ctx.content_rect().size();
            ui.set_width((sz.x * 0.8).min(1300.0));
            ui.horizontal(|ui| {
                ui.label(RichText::new(rel).strong().size(15.0));
                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    if ui.button("Close").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                        close_text = true;
                    }
                });
            });
            ui.separator();
            match res {
                Err(e) => {
                    ui.label(RichText::new(format!("Can't show a text diff: {e}")).color(WARN));
                }
                Ok(lines) => {
                    let (mut ln_l, mut ln_r) = (0usize, 0usize);
                    let numbered: Vec<(Option<usize>, Option<usize>, &Line)> = lines
                        .iter()
                        .map(|l| match l {
                            Line::Same(_) => {
                                ln_l += 1;
                                ln_r += 1;
                                (Some(ln_l), Some(ln_r), l)
                            }
                            Line::Removed(_) => {
                                ln_l += 1;
                                (Some(ln_l), None, l)
                            }
                            Line::Added(_) => {
                                ln_r += 1;
                                (None, Some(ln_r), l)
                            }
                        })
                        .collect();
                    let changes = lines.iter().filter(|l| !matches!(l, Line::Same(_))).count();
                    ui.label(RichText::new(format!("{changes} changed line(s) · unchanged stretches are collapsed")).weak());
                    // Keep 3 lines of context around changes; collapse long unchanged runs.
                    const CTX: usize = 3;
                    let is_change: Vec<bool> = numbered.iter().map(|x| !matches!(x.2, Line::Same(_))).collect();
                    let mut keep = vec![false; numbered.len()];
                    for (i, &c) in is_change.iter().enumerate() {
                        if c {
                            for k in i.saturating_sub(CTX)..(i + CTX + 1).min(numbered.len()) {
                                keep[k] = true;
                            }
                        }
                    }
                    let mut shown: Vec<Option<usize>> = Vec::new(); // None = "… N unchanged lines"
                    let mut i = 0;
                    while i < numbered.len() {
                        if keep[i] {
                            shown.push(Some(i));
                            i += 1;
                        } else {
                            let j = (i..numbered.len()).find(|&k| keep[k]).unwrap_or(numbered.len());
                            shown.push(None);
                            shown.push(Some(usize::MAX - (j - i))); // encoded count
                            i = j;
                        }
                    }
                    let numbered_ref = &numbered;
                    ui.spacing_mut().item_spacing.y = 0.0;
                    let rows: Vec<usize> = shown.iter().filter_map(|x| *x).collect();
                    egui::ScrollArea::both().max_height(sz.y * 0.7).auto_shrink([false, true]).show_rows(ui, 16.0, rows.len(), |ui, range| {
                        for &r in &rows[range] {
                            if r > numbered_ref.len() {
                                let n = usize::MAX - r;
                                ui.label(RichText::new(format!("            ⋯ {n} unchanged line(s)")).monospace().size(12.0).color(Color32::from_gray(110)));
                                continue;
                            }
                            let (a, b, l) = &numbered_ref[r];
                            let (txt, col, bg, sign) = match l {
                                Line::Same(s) => (s, Color32::from_gray(190), Color32::TRANSPARENT, " "),
                                Line::Removed(s) => (s, Color32::from_rgb(255, 170, 170), DANGER.gamma_multiply(0.18), "−"),
                                Line::Added(s) => (s, Color32::from_rgb(170, 240, 190), OK.gamma_multiply(0.18), "+"),
                            };
                            let num = |n: &Option<usize>| n.map(|x| format!("{x:>5}")).unwrap_or_else(|| "     ".into());
                            egui::Frame::new().fill(bg).show(ui, |ui| {
                                ui.set_min_width(ui.available_width());
                                ui.label(RichText::new(format!("{} {} {} {}", num(a), num(b), sign, txt)).monospace().size(12.0).color(col));
                            });
                        }
                    });
                }
            }
        });
        if mr.should_close() {
            close_text = true;
        }
    }
    if close_text {
        cs.text = None;
    }

    // report
    let mut close_report = false;
    if let Some(r) = &cs.report {
        let mr = egui::Modal::new(egui::Id::unique("cmp_report")).show(ctx, |ui| {
            ui.set_width(460.0);
            ui.label(r);
            if ui.button("OK").clicked() || ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                close_report = true;
            }
        });
        if mr.should_close() {
            close_report = true;
        }
    }
    if close_report {
        cs.report = None;
    }
}
