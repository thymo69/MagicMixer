//! The Magic Mixer icon, drawn in code so there are no image files to keep in
//! sync: two patch points joined by a cable on a rounded dark square.
//! Used for the window icon and written out as a .ico for shortcuts.

/// RGBA pixels, `size` x `size`, row-major from the top.
pub fn rgba(size: u32) -> Vec<u8> {
    let s = size as f32;
    let mut px = vec![0u8; (size * size * 4) as usize];

    // Cable: cubic bezier from left node to right node, sampled densely.
    let (p0, p3) = ((0.27, 0.34), (0.73, 0.66));
    let (p1, p2) = ((0.55, 0.34), (0.45, 0.66));
    let curve: Vec<(f32, f32)> = (0..=200)
        .map(|i| {
            let t = i as f32 / 200.0;
            let u = 1.0 - t;
            let x = u * u * u * p0.0 + 3.0 * u * u * t * p1.0 + 3.0 * u * t * t * p2.0 + t * t * t * p3.0;
            let y = u * u * u * p0.1 + 3.0 * u * u * t * p1.1 + 3.0 * u * t * t * p2.1 + t * t * t * p3.1;
            (x, y)
        })
        .collect();

    let bg = [0x1f, 0x2a, 0x44];
    let orange = [0xff, 0x8a, 0x3d];
    let green = [0x3d, 0xc9, 0x8a];
    let radius = 0.22; // corner radius, as a fraction of size
    let aa = 1.0 / s; // ~1px antialiasing

    for y in 0..size {
        for x in 0..size {
            let (fx, fy) = ((x as f32 + 0.5) / s, (y as f32 + 0.5) / s);

            // Rounded-square coverage.
            let dx = (fx - 0.5).abs() - (0.5 - radius);
            let dy = (fy - 0.5).abs() - (0.5 - radius);
            let outside = (dx.max(0.0).powi(2) + dy.max(0.0).powi(2)).sqrt() + dx.max(dy).min(0.0) - radius;
            let cover = (0.5 - outside / aa).clamp(0.0, 1.0);
            if cover == 0.0 {
                continue;
            }

            let mut c = [bg[0] as f32, bg[1] as f32, bg[2] as f32];
            let mut blend = |col: [u8; 3], a: f32| {
                for k in 0..3 {
                    c[k] = c[k] * (1.0 - a) + col[k] as f32 * a;
                }
            };

            // Cable, coloured orange -> green along its length.
            let (mut best, mut best_t) = (f32::MAX, 0.0);
            for (i, &(cx, cy)) in curve.iter().enumerate() {
                let d = ((fx - cx).powi(2) + (fy - cy).powi(2)).sqrt();
                if d < best {
                    best = d;
                    best_t = i as f32 / 200.0;
                }
            }
            let a = (0.5 - (best - 0.045) / aa).clamp(0.0, 1.0);
            if a > 0.0 {
                let col = [0, 1, 2].map(|k| (orange[k] as f32 * (1.0 - best_t) + green[k] as f32 * best_t) as u8);
                blend(col, a);
            }

            // Nodes.
            for (n, col) in [(p0, orange), (p3, green)] {
                let d = ((fx - n.0).powi(2) + (fy - n.1).powi(2)).sqrt();
                blend(col, (0.5 - (d - 0.10) / aa).clamp(0.0, 1.0));
                blend([0xff, 0xff, 0xff], (0.5 - (d - 0.04) / aa).clamp(0.0, 1.0) * 0.9);
            }

            let i = ((y * size + x) * 4) as usize;
            px[i] = c[0] as u8;
            px[i + 1] = c[1] as u8;
            px[i + 2] = c[2] as u8;
            px[i + 3] = (cover * 255.0) as u8;
        }
    }
    px
}

/// A Windows .ico with 16, 32, 48 and 256 px images (32-bit BMP entries).
pub fn ico_bytes() -> Vec<u8> {
    let sizes = [16u32, 32, 48, 256];
    let images: Vec<Vec<u8>> = sizes.iter().map(|&sz| bmp_entry(sz)).collect();
    let mut out = Vec::new();
    out.extend_from_slice(&[0, 0, 1, 0]);
    out.extend_from_slice(&(sizes.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * sizes.len() as u32;
    for (sz, img) in sizes.iter().zip(&images) {
        let b = if *sz >= 256 { 0 } else { *sz as u8 };
        out.extend_from_slice(&[b, b, 0, 0]);
        out.extend_from_slice(&1u16.to_le_bytes()); // planes
        out.extend_from_slice(&32u16.to_le_bytes()); // bpp
        out.extend_from_slice(&(img.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += img.len() as u32;
    }
    for img in images {
        out.extend_from_slice(&img);
    }
    out
}

fn bmp_entry(size: u32) -> Vec<u8> {
    let px = rgba(size);
    let mask_row = size.div_ceil(32) * 4;
    let mut out = Vec::new();
    // BITMAPINFOHEADER; height is doubled (image + AND mask) per the ICO format.
    for v in [40u32, size, size * 2] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    for _ in 0..6 {
        out.extend_from_slice(&0u32.to_le_bytes());
    }
    // Pixels bottom-up, BGRA.
    for y in (0..size).rev() {
        for x in 0..size {
            let i = ((y * size + x) * 4) as usize;
            out.extend_from_slice(&[px[i + 2], px[i + 1], px[i], px[i + 3]]);
        }
    }
    // AND mask: all zero (alpha channel does the work).
    out.extend(std::iter::repeat(0u8).take((mask_row * size) as usize));
    out
}
