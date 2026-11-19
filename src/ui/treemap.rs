//! Squarified, cushion-shaded treemap.
//!
//! Layout runs only when the root, size metric or canvas size changes. The result is
//! rasterised on the CPU (WinDirStat-style cushion shading) into one RGBA image that is
//! uploaded as a single texture — no per-frame shape spam even with 100k rectangles.
//! Large folders get a SpaceSniffer-style title strip so you can read the structure.

use crate::model::Tree;
use egui::Color32;

pub const HEADER_H: f32 = 16.0;

#[derive(Clone, Copy, Debug)]
pub struct Rect {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
}

impl Rect {
    #[inline]
    pub fn w(&self) -> f32 {
        self.x1 - self.x0
    }
    #[inline]
    pub fn h(&self) -> f32 {
        self.y1 - self.y0
    }
    #[inline]
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x0 && x < self.x1 && y >= self.y0 && y < self.y1
    }
}

#[derive(Clone, Debug)]
pub struct Cell {
    pub rect: Rect,
    pub node: u32,
    pub depth: u16,
    pub is_dir: bool,
    /// Folder with a visible title strip.
    pub header: bool,
    /// Cushion surface coefficients (WinDirStat algorithm).
    pub surface: [f32; 4],
}

pub struct Layout {
    pub cells: Vec<Cell>,
    pub width: usize,
    pub height: usize,
}

pub struct Params {
    pub use_alloc: bool,
    /// Draw folder title strips (SpaceSniffer style) when there is room.
    pub headers: bool,
    pub max_cells: usize,
}

#[inline]
fn weight(t: &Tree, i: u32, use_alloc: bool) -> f64 {
    let n = t.node(i);
    if use_alloc { n.alloc as f64 } else { n.size as f64 }
}

fn add_ridge(s: &mut [f32; 4], x0: f32, x1: f32, y0: f32, y1: f32, h: f32) {
    let w = x1 - x0;
    if w > 0.0 {
        s[2] -= 4.0 * h / w;
        s[0] += 4.0 * h * (x1 + x0) / w;
    }
    let hh = y1 - y0;
    if hh > 0.0 {
        s[3] -= 4.0 * h / hh;
        s[1] += 4.0 * h * (y1 + y0) / hh;
    }
}

pub fn layout(t: &Tree, root: u32, width: usize, height: usize, p: &Params) -> Layout {
    let mut cells = Vec::with_capacity(4096);
    let r = Rect { x0: 0.0, y0: 0.0, x1: width as f32, y1: height as f32 };
    let mut surface = [0.0f32; 4];
    let h0 = 0.38f32;
    add_ridge(&mut surface, r.x0, r.x1, r.y0, r.y1, h0);
    layout_node(t, root, r, 0, surface, h0, p, &mut cells, true);
    Layout { cells, width, height }
}

#[allow(clippy::too_many_arguments)]
fn layout_node(
    t: &Tree,
    i: u32,
    r: Rect,
    depth: u16,
    surface: [f32; 4],
    h: f32,
    p: &Params,
    out: &mut Vec<Cell>,
    is_root: bool,
) {
    let n = t.node(i);
    let is_dir = n.is_dir();
    let header = is_dir && p.headers && !is_root && r.w() >= 60.0 && r.h() >= 44.0;
    out.push(Cell { rect: r, node: i, depth, is_dir, header, surface });
    if !is_dir || out.len() >= p.max_cells {
        return;
    }
    // Inner area for children.
    let mut inner = r;
    if header {
        inner.y0 += HEADER_H;
        inner.x0 += 2.0;
        inner.x1 -= 2.0;
        inner.y1 -= 2.0;
    } else if !is_root && r.w() > 12.0 && r.h() > 12.0 {
        inner.x0 += 1.0;
        inner.y0 += 1.0;
        inner.x1 -= 1.0;
        inner.y1 -= 1.0;
    }
    if inner.w() < 2.0 || inner.h() < 2.0 {
        return;
    }
    let total = weight(t, i, p.use_alloc);
    if total <= 0.0 {
        return;
    }
    let mut kids: Vec<(u32, f64)> = t.children(i).map(|c| (c, weight(t, c, p.use_alloc))).filter(|&(_, w)| w > 0.0).collect();
    kids.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    if kids.is_empty() {
        return;
    }
    let kid_total: f64 = kids.iter().map(|k| k.1).sum();
    let area = (inner.w() * inner.h()) as f64;
    let px_per_unit = area / kid_total;

    // Drop children that would be smaller than ~3 px²; they stay as parent background.
    let min_w = 3.0 / px_per_unit;
    let visible: Vec<(u32, f64)> = kids.into_iter().filter(|k| k.1 >= min_w).collect();
    if visible.is_empty() {
        return;
    }

    let h_child = h * 0.75;
    squarify(&visible, inner, px_per_unit, &mut |c, cr| {
        let mut s = surface;
        add_ridge(&mut s, cr.x0, cr.x1, cr.y0, cr.y1, h_child);
        layout_node(t, c, cr, depth + 1, s, h_child, p, out, false);
    });
}

/// Squarified treemap (Bruls, Huizing, van Wijk). Items must be sorted descending.
fn squarify(items: &[(u32, f64)], mut r: Rect, px_per_unit: f64, emit: &mut dyn FnMut(u32, Rect)) {
    let mut i = 0;
    while i < items.len() {
        let w = r.w() as f64;
        let h = r.h() as f64;
        if w < 1.0 || h < 1.0 {
            return;
        }
        let side = w.min(h);
        // grow row while aspect ratio improves
        let mut row_sum = 0.0;
        let mut j = i;
        let mut best = f64::INFINITY;
        while j < items.len() {
            let a = items[j].1 * px_per_unit;
            let s2 = row_sum + a;
            let maxa = items[i].1 * px_per_unit; // sorted desc: first is max
            let mina = a;
            let worst = ((side * side * maxa) / (s2 * s2)).max((s2 * s2) / (side * side * mina));
            if worst > best {
                break;
            }
            best = worst;
            row_sum = s2;
            j += 1;
        }
        let thickness = (row_sum / side) as f32;
        let horizontal = w >= h; // lay row along the shorter side
        let mut pos = 0.0f32;
        for (k, &(node, wt)) in items[i..j].iter().enumerate() {
            let len = ((wt * px_per_unit) / row_sum * side) as f32;
            let last = k + 1 == j - i;
            let cr = if horizontal {
                // column on the left
                let y0 = r.y0 + pos;
                let y1 = if last { r.y1 } else { y0 + len };
                Rect { x0: r.x0, y0, x1: (r.x0 + thickness).min(r.x1), y1 }
            } else {
                let x0 = r.x0 + pos;
                let x1 = if last { r.x1 } else { x0 + len };
                Rect { x0, y0: r.y0, x1, y1: (r.y0 + thickness).min(r.y1) }
            };
            pos += len;
            emit(node, cr);
        }
        if horizontal {
            r.x0 += thickness;
        } else {
            r.y0 += thickness;
        }
        i = j;
    }
}

/// Rasterise the layout with cushion shading. `color_of` gives the base colour of a file cell.
pub fn rasterize(l: &Layout, t: &Tree, dark: bool, color_of: &dyn Fn(u32) -> Color32) -> egui::ColorImage {
    let (w, h) = (l.width.max(1), l.height.max(1));
    let bg = if dark { Color32::from_rgb(28, 31, 38) } else { Color32::from_rgb(232, 235, 240) };
    let mut px = vec![bg; w * h];
    // light source (top-left)
    let (lx, ly, lz) = (-1.0f32, -1.0f32, 10.0f32);
    let ln = (lx * lx + ly * ly + lz * lz).sqrt();
    let (lx, ly, lz) = (lx / ln, ly / ln, lz / ln);
    let ia = 0.42f32; // ambient
    let is = 1.0 - ia;

    for c in &l.cells {
        let x0 = c.rect.x0.round().max(0.0) as usize;
        let y0 = c.rect.y0.round().max(0.0) as usize;
        let x1 = (c.rect.x1.round() as usize).min(w);
        let y1 = (c.rect.y1.round() as usize).min(h);
        if x1 <= x0 || y1 <= y0 {
            continue;
        }
        if c.is_dir {
            // folder background / frame
            let depth_shade = (c.depth as i32 * 6).min(40) as u8;
            let fill = if dark {
                Color32::from_rgb(40 + depth_shade / 2, 44 + depth_shade / 2, 54 + depth_shade / 2)
            } else {
                Color32::from_rgb(200 - depth_shade, 205 - depth_shade, 214 - depth_shade)
            };
            for y in y0..y1 {
                px[y * w + x0..y * w + x1].fill(fill);
            }
            if c.header {
                let hy1 = (y0 + HEADER_H as usize).min(y1);
                let hc = if dark { Color32::from_rgb(58, 64, 80) } else { Color32::from_rgb(170, 178, 194) };
                for y in y0..hy1 {
                    px[y * w + x0..y * w + x1].fill(hc);
                }
            }
            continue;
        }
        let base = color_of(c.node);
        let (br, bgc, bb) = (base.r() as f32, base.g() as f32, base.b() as f32);
        let s = c.surface;
        for y in y0..y1 {
            let fy = y as f32 + 0.5;
            let ny = -(2.0 * s[3] * fy + s[1]);
            let row = &mut px[y * w..y * w + w];
            for (x, p) in row.iter_mut().enumerate().take(x1).skip(x0) {
                let fx = x as f32 + 0.5;
                let nx = -(2.0 * s[2] * fx + s[0]);
                let cosa = (nx * lx + ny * ly + lz) / (nx * nx + ny * ny + 1.0).sqrt();
                let v = (ia + is * cosa.max(0.0)).min(1.25);
                *p = Color32::from_rgb((br * v).min(255.0) as u8, (bgc * v).min(255.0) as u8, (bb * v).min(255.0) as u8);
            }
        }
    }
    let _ = t;
    egui::ColorImage::new([w, h], px)
}

/// Deepest cell under the point.
pub fn hit(l: &Layout, x: f32, y: f32) -> Option<&Cell> {
    l.cells.iter().rev().find(|c| c.rect.contains(x, y))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{RawEntry, ScanMethod, build_tree, flags};

    #[test]
    fn areas_are_proportional_and_inside() {
        let mut v = vec![RawEntry { id: 0, parent: 0, flags: flags::DIR, ..Default::default() }];
        for k in 1..200u32 {
            v.push(RawEntry { id: k, parent: 0, name: format!("f{k}.txt"), size: k as u64 * 1000, alloc: k as u64 * 1000, ..Default::default() });
        }
        let t = build_tree(v, 200, 0, "R", ScanMethod::Walk);
        let l = layout(&t, t.root, 800, 600, &Params { use_alloc: true, headers: true, max_cells: 100_000 });
        let total: f32 = l.cells.iter().filter(|c| !c.is_dir).map(|c| c.rect.w() * c.rect.h()).sum();
        assert!((total - 800.0 * 600.0).abs() / (800.0 * 600.0) < 0.02, "covered {total}");
        for c in &l.cells {
            assert!(c.rect.x0 >= -0.01 && c.rect.y0 >= -0.01 && c.rect.x1 <= 800.01 && c.rect.y1 <= 600.01);
        }
        let img = rasterize(&l, &t, true, &|_| Color32::from_rgb(200, 100, 50));
        assert_eq!(img.size, [800, 600]);
        assert!(hit(&l, 10.0, 10.0).is_some());
    }
}
