//! Sunburst ("disk fan") view: concentric rings, one per folder level.

use crate::model::Tree;
use egui::{Color32, Mesh, Pos2, Vec2, epaint::Vertex};
use std::f32::consts::TAU;

#[derive(Clone, Debug)]
pub struct Seg {
    pub node: u32,
    pub ring: u16,
    pub a0: f32,
    pub a1: f32,
}

pub struct Layout {
    pub segs: Vec<Seg>,
    pub rings: u16,
}

pub const MAX_RINGS: u16 = 7;
const MIN_ANGLE: f32 = 0.006; // ~0.35°

pub fn layout(t: &Tree, root: u32, use_alloc: bool) -> Layout {
    let mut segs = Vec::new();
    let mut rings = 0;
    rec(t, root, 0, 0.0, TAU, use_alloc, &mut segs, &mut rings);
    Layout { segs, rings }
}

#[allow(clippy::too_many_arguments)]
fn rec(t: &Tree, i: u32, ring: u16, a0: f32, a1: f32, use_alloc: bool, out: &mut Vec<Seg>, rings: &mut u16) {
    if ring >= MAX_RINGS {
        return;
    }
    let n = t.node(i);
    let total = if use_alloc { n.alloc } else { n.size } as f64;
    if total <= 0.0 {
        return;
    }
    let span = (a1 - a0) as f64;
    let mut a = a0 as f64;
    for c in t.children(i) {
        let cn = t.node(c);
        let w = if use_alloc { cn.alloc } else { cn.size } as f64;
        let da = span * w / total;
        if (da as f32) < MIN_ANGLE {
            break; // sorted descending: everything after is smaller
        }
        out.push(Seg { node: c, ring, a0: a as f32, a1: (a + da) as f32 });
        *rings = (*rings).max(ring + 1);
        if cn.is_dir() {
            rec(t, c, ring + 1, a as f32, (a + da) as f32, use_alloc, out, rings);
        }
        a += da;
    }
}

pub struct Geometry {
    pub center: Pos2,
    pub r0: f32,
    pub ring_w: f32,
}

pub fn geometry(rect: egui::Rect, rings: u16) -> Geometry {
    let radius = rect.width().min(rect.height()) * 0.5 - 8.0;
    let rings = rings.max(1) as f32;
    let r0 = (radius * 0.22).max(30.0);
    let ring_w = ((radius - r0) / rings).max(4.0);
    Geometry { center: rect.center(), r0, ring_w }
}

pub fn mesh(l: &Layout, g: &Geometry, color_of: &dyn Fn(&Seg) -> Color32, highlight: Option<u32>) -> Mesh {
    let mut m = Mesh::default();
    for s in &l.segs {
        let ri = g.r0 + s.ring as f32 * g.ring_w + 1.0;
        let ro = ri + g.ring_w - 2.0;
        let gap = (0.8 / ro).min((s.a1 - s.a0) * 0.2);
        let (a0, a1) = (s.a0 + gap, s.a1 - gap);
        if a1 <= a0 {
            continue;
        }
        let mut col = color_of(s);
        if highlight == Some(s.node) {
            col = col.lerp_to_gamma(Color32::WHITE, 0.35);
        }
        let steps = (((a1 - a0) * ro / 3.0).ceil() as usize).clamp(1, 256);
        let base = m.vertices.len() as u32;
        for k in 0..=steps {
            let a = a0 + (a1 - a0) * k as f32 / steps as f32 - TAU / 4.0;
            let dir = Vec2::new(a.cos(), a.sin());
            // subtle radial gradient: inner edge darker
            let inner = col.lerp_to_gamma(Color32::BLACK, 0.18);
            m.vertices.push(Vertex { pos: g.center + dir * ri, uv: egui::epaint::WHITE_UV, color: inner });
            m.vertices.push(Vertex { pos: g.center + dir * ro, uv: egui::epaint::WHITE_UV, color: col });
        }
        for k in 0..steps as u32 {
            let i = base + k * 2;
            m.indices.extend_from_slice(&[i, i + 1, i + 2, i + 1, i + 3, i + 2]);
        }
    }
    m
}

pub enum Hit<'a> {
    Center,
    Seg(&'a Seg),
}

pub fn hit<'a>(l: &'a Layout, g: &Geometry, p: Pos2) -> Option<Hit<'a>> {
    let d = p - g.center;
    let r = d.length();
    if r < g.r0 {
        return Some(Hit::Center);
    }
    let ring = ((r - g.r0) / g.ring_w) as u16;
    let mut a = d.y.atan2(d.x) + TAU / 4.0;
    if a < 0.0 {
        a += TAU;
    }
    l.segs.iter().find(|s| s.ring == ring && a >= s.a0 && a < s.a1).map(Hit::Seg)
}
