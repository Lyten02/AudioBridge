//! App/tray icon drawn in code: a rounded square with a sound-wave glyph.
//! Green when a phone is connected, slate grey otherwise.

/// Straight (non-premultiplied) RGBA, row-major, top-down.
pub fn render(size: u32, connected: bool) -> Vec<u8> {
    let bg: [f32; 3] = if connected { [0.13, 0.77, 0.47] } else { [0.36, 0.40, 0.48] };
    let s = size as f32;
    let radius = s * 0.24;
    // Five vertical bars, symmetric heights, in unit coordinates.
    let bars: [(f32, f32); 5] = [(0.24, 0.22), (0.37, 0.46), (0.50, 0.64), (0.63, 0.46), (0.76, 0.22)];
    let bar_w = 0.085;
    const SS: u32 = 4; // supersampling per axis
    let mut out = vec![0u8; (size * size * 4) as usize];
    for y in 0..size {
        for x in 0..size {
            let mut cover_bg = 0.0f32;
            let mut cover_fg = 0.0f32;
            for sy in 0..SS {
                for sx in 0..SS {
                    let px = x as f32 + (sx as f32 + 0.5) / SS as f32;
                    let py = y as f32 + (sy as f32 + 0.5) / SS as f32;
                    if !in_round_rect(px, py, s, radius) {
                        continue;
                    }
                    cover_bg += 1.0;
                    let (u, v) = (px / s, py / s);
                    let on_bar = bars.iter().any(|&(cx, h)| {
                        let half_h = h / 2.0;
                        let dx = (u - cx).abs();
                        let dy = (v - 0.5).abs();
                        // Capsule: rectangle with round caps.
                        let r = bar_w / 2.0;
                        if dx > r {
                            return false;
                        }
                        let core = (half_h - r).max(0.0);
                        dy <= core || (dx * dx + (dy - core) * (dy - core)) <= r * r
                    });
                    if on_bar {
                        cover_fg += 1.0;
                    }
                }
            }
            let n = (SS * SS) as f32;
            let a = cover_bg / n;
            if a <= 0.0 {
                continue;
            }
            let f = if cover_bg > 0.0 { cover_fg / cover_bg } else { 0.0 };
            let i = ((y * size + x) * 4) as usize;
            for c in 0..3 {
                out[i + c] = ((bg[c] * (1.0 - f) + f) * 255.0).round() as u8;
            }
            out[i + 3] = (a * 255.0).round() as u8;
        }
    }
    out
}

fn in_round_rect(px: f32, py: f32, s: f32, r: f32) -> bool {
    let cx = px.clamp(r, s - r);
    let cy = py.clamp(r, s - r);
    let (dx, dy) = (px - cx, py - cy);
    dx * dx + dy * dy <= r * r
}
