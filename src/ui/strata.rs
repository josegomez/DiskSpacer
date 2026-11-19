//! "Disk Strata": a zoomable icicle chart.
//!
//! Row 0 is the current folder (or the whole drive, with free space) as one full-width
//! bar. Every row below splits its parent block into children, width ∝ size. Labels are
//! horizontal and legible; zooming animates the visible window. Everything is laid out
//! per frame in absolute "fraction of the drive" coordinates, so a zoom is just an
//! animated change of the visible window.

use super::{Action, ColorMode, State, chart_context_menu, file_cat_color, n_metric, node_tooltip};
use crate::category;
use crate::model::{NONE, Tree};
use crate::util::fmt_size;
use egui::{Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};
use std::time::Instant;

const GAP: f32 = 3.0;
const CRUMB: f32 = 18.0;
const MIN_W: f32 = 6.0;
const ANIM_SECS: f32 = 0.30;

pub struct Anim {
    from: (f64, f64, f32),
    start: Instant,
}

type Kids = std::rc::Rc<Vec<(u32, u64)>>;

#[derive(Default)]
pub struct StrataState {
    /// Sorted children per folder, cached for the current (generation, metric):
    /// avoids re-collecting huge folders (100k+ entries) every frame.
    kid_cache: std::cell::RefCell<(u64, bool, std::collections::HashMap<u32, Kids>)>,
    /// Last displayed (window start, window end, depth offset).
    shown: Option<(f64, f64, f32)>,
    target_root: u32,
    anim: Option<Anim>,
}

/// Children sorted by the active metric (cached).
fn kids(t: &Tree, st: &State, i: u32) -> Kids {
    let mut c = st.strata.kid_cache.borrow_mut();
    if c.0 != st.generation || c.1 != st.use_alloc {
        *c = (st.generation, st.use_alloc, Default::default());
    }
    c.2.entry(i).or_insert_with(|| std::rc::Rc::new(kids_uncached(t, i, st.use_alloc))).clone()
}

fn kids_uncached(t: &Tree, i: u32, use_alloc: bool) -> Vec<(u32, u64)> {
    let mut v: Vec<(u32, u64)> = t.children(i).map(|c| (c, n_metric(t, c, use_alloc))).filter(|k| k.1 > 0).collect();
    // always sort: children added by live updates are appended unsorted
    v.sort_by(|a, b| b.1.cmp(&a.1));
    v
}

fn depth_of(t: &Tree, i: u32) -> u32 {
    t.ancestry(i).len() as u32 - 1
}

/// Fraction of the drive the used part occupies (the rest of row 0 is free space).
fn used_span(t: &Tree, st: &State) -> f64 {
    if st.show_free && st.use_alloc && t.volume_total > 0 && crate::platform::drive_letter_root(std::path::Path::new(t.name(t.root))).is_some() {
        let used = t.node(t.root).alloc as f64;
        let total = used + t.volume_free as f64;
        if total > 0.0 {
            return used / total;
        }
    }
    1.0
}

/// Absolute interval of `node` within [0, 1].
fn interval(t: &Tree, st: &State, node: u32) -> (f64, f64) {
    let chain = t.ancestry(node);
    let (mut a0, mut a1) = (0.0f64, used_span(t, st));
    for w in chain.windows(2) {
        let (p, c) = (w[0], w[1]);
        let total = n_metric(t, p, st.use_alloc) as f64;
        if total <= 0.0 {
            break;
        }
        let mut off = 0u64;
        let mut wc = 0u64;
        for &(k, wt) in kids(t, st, p).iter() {
            if k == c {
                wc = wt;
                break;
            }
            off += wt;
        }
        let span = a1 - a0;
        let n0 = a0 + span * off as f64 / total;
        a1 = n0 + span * wc as f64 / total;
        a0 = n0;
    }
    (a0, a1)
}

fn ease(x: f32) -> f32 {
    1.0 - (1.0 - x).powi(3)
}

// ---------- colour ----------

/// OKLCH → sRGB (perceptually even hues).
pub fn oklch(l: f32, c: f32, h_deg: f32) -> Color32 {
    let h = h_deg.to_radians();
    let (a, b) = (c * h.cos(), c * h.sin());
    let l_ = l + 0.396_337_8 * a + 0.215_803_76 * b;
    let m_ = l - 0.105_561_35 * a - 0.063_854_17 * b;
    let s_ = l - 0.089_484_18 * a - 1.291_485_5 * b;
    let (l3, m3, s3) = (l_ * l_ * l_, m_ * m_ * m_, s_ * s_ * s_);
    let r = 4.076_741_7 * l3 - 3.307_711_6 * m3 + 0.230_969_94 * s3;
    let g = -1.268_438 * l3 + 2.609_757_4 * m3 - 0.341_319_38 * s3;
    let bl = -0.004_196_086_3 * l3 - 0.703_418_6 * m3 + 1.707_614_7 * s3;
    let enc = |x: f32| egui::ecolor::gamma_u8_from_linear_f32(x.clamp(0.0, 1.0));
    Color32::from_rgb(enc(r), enc(g), enc(bl))
}

fn luminance(c: Color32) -> f32 {
    (0.299 * c.r() as f32 + 0.587 * c.g() as f32 + 0.114 * c.b() as f32) / 255.0
}

pub const BRANCH_HUES: [f32; 10] = [255.0, 25.0, 150.0, 70.0, 300.0, 200.0, 105.0, 345.0, 230.0, 45.0];

struct Paint<'a> {
    t: &'a Tree,
    st: &'a State,
    win: (f64, f64),
    dview: f32,
    area: Rect,
    rows: f32,
    row_h: f32,
    /// Depth whose children get distinct branch hues (first level that actually splits).
    hue_depth: u32,
    hovered_prev: Option<u32>,
    hit: Option<(u32, bool, u32)>, // (node or parent, is_aggregate, aggregated count)
    hit_rect: Option<Rect>,
    pointer: Option<Pos2>,
    shapes: Vec<egui::Shape>,
    labels: Vec<(Rect, String, String, Color32, bool)>,
    cats: &'a [category::Category],
}

impl Paint<'_> {
    fn sx(&self, a: f64) -> f32 {
        let (w0, w1) = self.win;
        self.area.left() + ((a - w0) / (w1 - w0)) as f32 * self.area.width()
    }

    fn related(&self, b: u32) -> bool {
        match self.hovered_prev {
            None => true,
            Some(h) => b == h || self.t.is_ancestor(b, h) || self.t.is_ancestor(h, b),
        }
    }

    fn color(&self, node: u32, rel_depth: i32, hue: Option<f32>, sibling_idx: usize) -> Color32 {
        let t = self.t;
        let n = t.node(node);
        let alt = if sibling_idx % 2 == 1 { 0.022 } else { 0.0 };
        let base = match self.st.color_mode {
            ColorMode::Type => {
                if n.is_dir() {
                    oklch(0.36 + 0.035 * rel_depth.max(0) as f32 + alt, 0.025, 255.0)
                } else {
                    file_cat_color(t, self.st, self.cats, node)
                }
            }
            ColorMode::Age => {
                let days = ((self.st.now - n.modified).max(0) as f32) / 86400.0;
                let k = ((days + 1.0).ln() / 1826f32.ln()).clamp(0.0, 1.0);
                // recent = warm amber, old = deep blue
                oklch(0.78 - 0.30 * k + alt, 0.14, 70.0 + 190.0 * k)
            }
            ColorMode::Folder => match hue {
                None => oklch(0.40 + alt, 0.03, 255.0),
                Some(h) => {
                    let l = (0.56 + 0.045 * (rel_depth - 1).max(0) as f32 + alt).min(0.86);
                    if n.is_dir() { oklch(l, 0.13, h) } else { oklch((l + 0.05).min(0.9), 0.07, h) }
                }
            },
        };
        let mut c = base;
        if let Some(cat) = self.st.highlight_cat
            && (n.is_dir() || category::of_node(t, self.cats, node) != cat)
        {
            c = c.lerp_to_gamma(Color32::from_rgb(34, 36, 42), if n.is_dir() { 0.45 } else { 0.8 });
        }
        if !self.related(node) {
            c = c.lerp_to_gamma(Color32::from_rgb(18, 20, 24), 0.45);
        }
        if self.hovered_prev == Some(node) {
            c = c.lerp_to_gamma(Color32::WHITE, 0.18);
        }
        c
    }

    fn block_rect(&self, x0: f32, x1: f32, row: f32) -> Rect {
        let y = self.area.top() + row * (self.row_h + GAP);
        Rect::from_min_max(pos2(x0 + 0.5, y), pos2(x1 - 0.5, y + self.row_h))
    }

    fn push_block(&mut self, r: Rect, fill: Color32, row: f32) {
        let vis = r.intersect(self.area);
        if vis.width() <= 0.0 || vis.height() <= 0.0 {
            return;
        }
        // fade the last row to hint there's more below
        let fade = if row > self.rows - 1.0 { (self.rows - row).clamp(0.25, 1.0) } else { 1.0 };
        let fill = if fade < 1.0 { fill.gamma_multiply(fade) } else { fill };
        let rad = if r.width() > 10.0 { 5.0 } else { 1.0 };
        self.shapes.push(egui::Shape::rect_filled(r, rad, fill));
        if r.width() > 6.0 {
            // subtle top highlight
            let hl = Rect::from_min_size(r.min + vec2(rad * 0.6, 0.5), vec2((r.width() - rad * 1.2).max(0.0), 1.0));
            self.shapes.push(egui::Shape::rect_filled(hl, 0.0, Color32::from_white_alpha(28)));
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn rec(&mut self, node: u32, a0: f64, a1: f64, depth: u32, hue: Option<f32>, sib: usize, view_depth: u32) {
        let x0 = self.sx(a0);
        let x1 = self.sx(a1);
        if x1 < self.area.left() || x0 > self.area.right() || x1 - x0 < 0.5 {
            return;
        }
        let row = depth as f32 - self.dview;
        if row > self.rows {
            return;
        }
        let rel = depth as i32 - view_depth as i32;
        if row > -1.0 {
            let r = self.block_rect(x0, x1, row);
            let fill = self.color(node, rel, hue, sib);
            self.push_block(r, fill, row);
            if let Some(p) = self.pointer
                && r.contains(p)
                && self.area.contains(p)
            {
                self.hit = Some((node, false, 0));
                self.hit_rect = Some(r);
            }
            if r.width() >= 26.0 && r.intersects(self.area) {
                let t = self.t;
                let txt_col = if luminance(fill) > 0.62 { Color32::from_rgb(20, 22, 26) } else { Color32::WHITE };
                let size = if r.width() >= 60.0 { fmt_size(n_metric(t, node, self.st.use_alloc)) } else { String::new() };
                let mut name = t.name(node).to_string();
                if node == t.root && name.len() > 3 {
                    name = super::short_name(t, node);
                }
                self.labels.push((r, name, size, txt_col, rel == 0));
            }
        }
        let t = self.t;
        if !t.node(node).is_dir() {
            return;
        }
        let total = n_metric(t, node, self.st.use_alloc) as f64;
        if total <= 0.0 {
            return;
        }
        let ks = kids(t, self.st, node);
        let span = a1 - a0;
        let mut off = 0u64;
        let px_per_unit = (x1 - x0) as f64 / total;
        for (k, &(c, w)) in ks.iter().enumerate() {
            let cw = w as f64 * px_per_unit;
            let c0 = a0 + span * off as f64 / total;
            if cw < MIN_W as f64 {
                // aggregate the (sorted) tail into one "smaller items" block
                let rest = ks.len() - k;
                let crow = row + 1.0;
                if crow > -1.0 && crow <= self.rows {
                    let ax0 = self.sx(c0);
                    let r = self.block_rect(ax0, x1, crow);
                    if r.width() >= 1.5 {
                        let mut base = Color32::from_rgb(64, 68, 80);
                        if self.st.color_mode == ColorMode::Type {
                            // tint by the dominant kind of file in the merged items
                            let mut by_cat = [0u64; 10];
                            for &(c, w) in ks[k..].iter().take(2000) {
                                if !t.node(c).is_dir() {
                                    by_cat[category::of_node(t, self.cats, c).index()] += w;
                                }
                            }
                            if let Some((ci, &w)) = by_cat.iter().enumerate().max_by_key(|x| x.1)
                                && w > 0
                            {
                                base = super::cat_color(category::ALL[ci]).lerp_to_gamma(Color32::from_rgb(40, 44, 54), 0.35);
                            }
                        }
                        let fill = base.lerp_to_gamma(Color32::from_rgb(18, 20, 24), if self.related(node) { 0.0 } else { 0.45 });
                        self.push_block(r, fill, crow);
                        // dotted texture
                        if r.width() > 8.0 {
                            let mut x = r.left() + 3.0;
                            while x < r.right() - 2.0 {
                                self.shapes.push(egui::Shape::circle_filled(pos2(x, r.bottom() - 5.0), 0.9, Color32::from_white_alpha(70)));
                                x += 5.0;
                            }
                        }
                        if let Some(p) = self.pointer
                            && r.contains(p)
                            && self.area.contains(p)
                        {
                            self.hit = Some((node, true, rest as u32));
                        }
                        if r.width() >= 70.0 {
                            let rest_size: u64 = (total as u64).saturating_sub(off);
                            self.labels.push((r, format!("{rest} smaller"), fmt_size(rest_size), Color32::from_gray(210), false));
                        }
                    }
                }
                break;
            }
            let child_hue = if depth == self.hue_depth {
                Some(BRANCH_HUES[k % BRANCH_HUES.len()])
            } else if depth > self.hue_depth {
                hue
            } else if depth >= view_depth {
                // single-child chain above the first real split: neutral blue-grey
                None
            } else if depth > view_depth {
                hue
            } else {
                None
            };
            self.rec(c, c0, c0 + span * w as f64 / total, depth + 1, child_hue, k, view_depth);
            off += w;
        }
    }
}

pub fn view(ui: &mut Ui, t: &Tree, st: &mut State, acts: &mut Vec<Action>) {
    let full = ui.available_rect_before_wrap();
    let resp = ui.allocate_rect(full, Sense::click());
    let painter = ui.painter_at(full);
    let cats = st.cats.clone();

    // ---- breadcrumb bars for the ancestors of the zoom root ----
    let chain = t.ancestry(st.view_root);
    let ancestors = &chain[..chain.len() - 1];
    let shown_anc: Vec<u32> = ancestors.iter().rev().take(4).rev().copied().collect();
    let mut y = full.top();
    for &a in &shown_anc {
        let r = Rect::from_min_size(pos2(full.left(), y), vec2(full.width(), CRUMB - 2.0));
        let hov = resp.hover_pos().is_some_and(|p| r.contains(p));
        painter.rect_filled(r, 4.0, if hov { Color32::from_rgb(62, 70, 90) } else { Color32::from_rgb(40, 44, 56) });
        let label = format!("{}   {}", super::short_name(t, a), fmt_size(n_metric(t, a, st.use_alloc)));
        painter.with_clip_rect(r).text(pos2(r.left() + 8.0, r.center().y), Align2::LEFT_CENTER, label, FontId::proportional(11.0), Color32::from_gray(200));
        if hov && resp.clicked() {
            acts.push(Action::Zoom(a));
        }
        y += CRUMB;
    }
    if !shown_anc.is_empty() {
        y += 4.0;
    }
    let area = Rect::from_min_max(pos2(full.left(), y), full.max);
    // Rows grow to use the space: aim for ~9 visible levels, 34–54 px each.
    let row_h = ((area.height() + GAP) / 9.0 - GAP).clamp(34.0, 54.0);
    let rows = ((area.height() + GAP) / (row_h + GAP)).floor().max(1.0) - 1.0;

    // ---- window & animation ----
    let target_win = interval(t, st, st.view_root);
    let target_depth = depth_of(t, st.view_root) as f32;
    let target = (target_win.0, target_win.1, target_depth);
    let ss = &mut st.strata;
    if ss.target_root != st.view_root || ss.shown.is_none() {
        if let Some(prev) = ss.shown
            && ss.target_root != st.view_root
        {
            if !st.remote {
                ss.anim = Some(Anim { from: prev, start: Instant::now() });
            }
        }
        ss.target_root = st.view_root;
    }
    let mut cur = target;
    if let Some(an) = &ss.anim {
        let k = (an.start.elapsed().as_secs_f32() / ANIM_SECS).min(1.0);
        let e = ease(k) as f64;
        cur = (
            an.from.0 + (target.0 - an.from.0) * e,
            an.from.1 + (target.1 - an.from.1) * e,
            an.from.2 + (target.2 - an.from.2) * e as f32,
        );
        if k >= 1.0 {
            ss.anim = None;
        } else {
            ui.ctx().request_repaint();
        }
    }
    ss.shown = Some(cur);

    // ---- paint ----
    let mut p = Paint {
        t,
        st,
        win: (cur.0, cur.1.max(cur.0 + 1e-12)),
        dview: cur.2,
        area,
        rows,
        row_h,
        hue_depth: {
            let mut d = target_depth as u32;
            let mut n = st.view_root;
            loop {
                let ks = kids(t, st, n);
                if ks.len() == 1 && t.node(ks[0].0).is_dir() {
                    n = ks[0].0;
                    d += 1;
                } else {
                    break d;
                }
            }
        },
        hovered_prev: if st.remote { None } else { st.hovered },
        hit: None,
        hit_rect: None,
        pointer: resp.hover_pos(),
        shapes: Vec::with_capacity(4096),
        labels: Vec::new(),
        cats: &cats,
    };
    let span = used_span(t, st);
    let view_depth = target_depth as u32;
    p.rec(t.root, 0.0, span, 0, None, 0, view_depth);
    // free space at the end of the drive bar
    if span < 1.0 {
        let fx0 = p.sx(span);
        let fx1 = p.sx(1.0);
        let row = 0.0 - p.dview;
        if row > -1.0 && fx1 > fx0 + 1.0 {
            let r = p.block_rect(fx0 + 2.0, fx1, row);
            p.shapes.push(egui::Shape::rect_filled(r, 5.0, Color32::from_rgb(30, 33, 40)));
            p.shapes.push(egui::Shape::rect_stroke(r, 5.0, Stroke::new(1.0, Color32::from_rgb(70, 76, 90)), StrokeKind::Inside));
            // hatching
            let clip = r.shrink(1.0);
            let mut x = clip.left() - clip.height();
            while x < clip.right() {
                let a = pos2(x, clip.bottom());
                let b = pos2(x + clip.height(), clip.top());
                p.shapes.push(egui::Shape::line_segment([a, b], Stroke::new(1.0, Color32::from_rgb(48, 52, 62))));
                x += 9.0;
            }
            if r.width() > 60.0 {
                p.labels.push((r, "Free".into(), fmt_size(t.volume_free), Color32::from_gray(170), true));
            }
        }
    }
    let Paint { shapes, labels, hit, hit_rect, .. } = p;
    painter.with_clip_rect(area).extend(shapes);

    // labels (clipped per block, measured with the real font)
    for (r, name, size, col, big) in labels {
        let clip = painter.with_clip_rect(r.shrink2(vec2(4.0, 0.0)).intersect(area));
        let f = FontId::proportional(if big { 15.0 } else { 12.5 });
        let fs = FontId::proportional(if big { 13.5 } else { 11.5 });
        let inner = r.width() - 14.0;
        let mut size_w = 0.0;
        if !size.is_empty() {
            size_w = ui.fonts_mut(|fo| fo.layout_no_wrap(size.clone(), fs.clone(), col).size().x);
            let name_w = ui.fonts_mut(|fo| fo.layout_no_wrap(name.clone(), f.clone(), col).size().x);
            // The name wins: only show the size if both fit, or the block is wide anyway.
            if name_w + size_w + 12.0 > inner && inner < 150.0 {
                size_w = 0.0;
            }
        }
        let name_max = if size_w > 0.0 { inner - size_w - 10.0 } else { inner };
        let shown = elide_fit(ui, &name, &f, name_max);
        if !shown.is_empty() {
            clip.text(pos2(r.left() + 7.0, r.center().y), Align2::LEFT_CENTER, shown, f, col);
        }
        if size_w > 0.0 {
            clip.text(pos2(r.right() - 7.0, r.center().y), Align2::RIGHT_CENTER, &size, fs, col.gamma_multiply(0.82));
        }
    }

    // Remote mode: a thin outline is the only hover feedback (tiny screen update).
    if st.remote
        && let Some(r) = hit_rect
    {
        painter.with_clip_rect(area).rect_stroke(r, 5.0, Stroke::new(1.5, Color32::from_white_alpha(220)), StrokeKind::Inside);
    }

    // selection outline
    if let Some(sel) = st.selected
        && sel != NONE
        && t.is_ancestor(t.root, sel)
    {
        let (a0, a1) = interval(t, st, sel);
        let row = depth_of(t, sel) as f32 - cur.2;
        if row > -1.0 && row <= rows {
            let w = (cur.0, cur.1.max(cur.0 + 1e-12));
            let sx = |a: f64| area.left() + ((a - w.0) / (w.1 - w.0)) as f32 * area.width();
            let y = area.top() + row * (row_h + GAP);
            let r = Rect::from_min_max(pos2(sx(a0), y), pos2(sx(a1), y + row_h));
            painter.with_clip_rect(area).rect_stroke(r, 5.0, Stroke::new(2.0, Color32::WHITE), StrokeKind::Outside);
        }
    }

    // ---- interaction ----
    if resp.clicked_by(egui::PointerButton::Extra1) {
        acts.push(Action::ZoomOut);
    }
    let prev_hover = st.hovered;
    st.hovered = None;
    if let Some((node, is_agg, count)) = hit {
        if !is_agg {
            st.hovered = Some(node);
            let resp = if st.remote { resp.clone() } else { resp.clone().on_hover_ui_at_pointer(|ui| node_tooltip(ui, t, node)) };
            if resp.clicked() {
                acts.push(Action::Select(node));
            }
            if resp.double_clicked() {
                if t.node(node).is_dir() {
                    acts.push(Action::Zoom(node));
                } else if t.node(node).parent != NONE {
                    acts.push(Action::Zoom(t.node(node).parent));
                }
            }
            if resp.secondary_clicked() {
                acts.push(Action::Select(node));
                st.menu_node = Some(node);
            }
        } else {
            let resp = resp.clone().on_hover_text(format!("{count} smaller items in {} — click to zoom in", t.name(node)));
            if resp.clicked() {
                acts.push(Action::Zoom(node));
            }
        }
    } else if resp.secondary_clicked() && st.menu_node.is_none() && st.view_root != t.root {
        acts.push(Action::ZoomOut);
    }
    chart_context_menu(&resp, t, st, acts);
    if st.hovered != prev_hover {
        ui.ctx().request_repaint(); // the info bar above was drawn with last frame's hover
    }
}

/// Longest prefix of `s` (with an ellipsis) that fits in `max_w` points.
fn elide_fit(ui: &Ui, s: &str, font: &FontId, max_w: f32) -> String {
    if max_w < 14.0 {
        return String::new();
    }
    let width = |t: &str| ui.fonts_mut(|f| f.layout_no_wrap(t.to_string(), font.clone(), Color32::WHITE).size().x);
    if width(s) <= max_w {
        return s.to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    let (mut lo, mut hi) = (0usize, chars.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let cand: String = chars[..mid].iter().collect::<String>() + "…";
        if width(&cand) <= max_w { lo = mid } else { hi = mid - 1 }
    }
    if lo == 0 {
        return String::new();
    }
    chars[..lo].iter().collect::<String>() + "…"
}
