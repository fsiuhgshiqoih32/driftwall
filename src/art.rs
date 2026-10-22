//! The app icon, drawn in code (used for the window and, through `examples/make_icon.rs`,
//! for the .ico embedded in the exe).

fn mix(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}

/// Inside a rounded square of half-size 0.5 and corner radius `r` (unit coordinates).
fn in_rounded(x: f32, y: f32, r: f32) -> bool {
    let qx = (x - 0.5).abs() - (0.5 - r);
    let qy = (y - 0.5).abs() - (0.5 - r);
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    outside + qx.max(qy).min(0.0) <= r
}

/// Height of the n-th wave at x (unit coordinates).
fn wave(x: f32, base: f32, amp: f32, phase: f32) -> f32 {
    base + amp * (x * std::f32::consts::TAU * 1.1 + phase).sin()
}

/// Straight RGBA pixels, `size` x `size`.
pub fn icon_rgba(size: usize) -> Vec<u8> {
    const SS: usize = 4;
    let top = [0.20, 0.78, 0.86];
    let bottom = [0.25, 0.30, 0.86];
    let mut out = vec![0u8; size * size * 4];
    for py in 0..size {
        for px in 0..size {
            let mut acc = [0.0f32; 4];
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = (px as f32 + (sx as f32 + 0.5) / SS as f32) / size as f32;
                    let y = (py as f32 + (sy as f32 + 0.5) / SS as f32) / size as f32;
                    if !in_rounded(x, y, 0.22) {
                        continue;
                    }
                    let mut c = mix(top, bottom, (y * 0.8 + x * 0.2).clamp(0.0, 1.0));
                    // Three drifting bands, lighter towards the top.
                    let bands = [
                        (0.42, 0.055, 0.0, 0.95),
                        (0.58, 0.05, 1.3, 0.7),
                        (0.74, 0.045, 2.6, 0.45),
                    ];
                    for (base, amp, phase, alpha) in bands {
                        let h = wave(x, base, amp, phase);
                        if y >= h && y <= h + 0.075 {
                            c = mix(c, [1.0, 1.0, 1.0], alpha);
                        }
                    }
                    acc[0] += c[0];
                    acc[1] += c[1];
                    acc[2] += c[2];
                    acc[3] += 1.0;
                }
            }
            let n = (SS * SS) as f32;
            let i = (py * size + px) * 4;
            if acc[3] > 0.0 {
                for k in 0..3 {
                    out[i + k] = ((acc[k] / acc[3]) * 255.0).round().clamp(0.0, 255.0) as u8;
                }
                out[i + 3] = ((acc[3] / n) * 255.0).round() as u8;
            }
        }
    }
    out
}
