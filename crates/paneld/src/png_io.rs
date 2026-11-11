//! PNG encoding for the preview and golden images.

use embedded_graphics::pixelcolor::RgbColor;

use crate::canvas::Canvas;

/// Encode `canvas` as an RGB PNG, each pixel a `scale × scale` block.
/// With `grid`, the last row and column of each block is darkened so the
/// image looks like individual LEDs (only when `scale >= 3`).
pub fn encode(canvas: &Canvas, scale: u32, grid: bool) -> Vec<u8> {
    let scale = scale.max(1);
    let (w, h) = (canvas.width() * scale, canvas.height() * scale);
    let grid = grid && scale >= 3;
    let mut data = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            let gap = grid && (x % scale == scale - 1 || y % scale == scale - 1);
            let p = canvas.pixel(x / scale, y / scale).expect("in bounds");
            if gap {
                data.extend_from_slice(&[p.r() / 8, p.g() / 8, p.b() / 8]);
            } else {
                data.extend_from_slice(&[p.r(), p.g(), p.b()]);
            }
        }
    }
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, w, h);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().expect("writing to a Vec cannot fail");
    writer
        .write_image_data(&data)
        .expect("writing to a Vec cannot fail");
    writer.finish().expect("writing to a Vec cannot fail");
    out
}

/// Decode an 8-bit RGB PNG into a canvas (used to compare golden images).
pub fn decode(bytes: &[u8]) -> anyhow::Result<Canvas> {
    let mut reader = png::Decoder::new(std::io::Cursor::new(bytes)).read_info()?;
    let mut buf = vec![0; reader.output_buffer_size().unwrap_or(0)];
    let info = reader.next_frame(&mut buf)?;
    anyhow::ensure!(
        info.color_type == png::ColorType::Rgb && info.bit_depth == png::BitDepth::Eight,
        "expected 8-bit RGB PNG, got {:?} {:?}",
        info.color_type,
        info.bit_depth
    );
    let mut canvas = Canvas::new(info.width, info.height);
    for y in 0..info.height {
        let row = &buf[(y as usize) * info.line_size..][..info.width as usize * 3];
        for (x, px) in row.chunks_exact(3).enumerate() {
            canvas.set_pixel(
                x as u32,
                y,
                embedded_graphics::pixelcolor::Rgb888::new(px[0], px[1], px[2]),
            );
        }
    }
    Ok(canvas)
}

#[cfg(test)]
mod tests {
    use super::*;
    use embedded_graphics::pixelcolor::Rgb888;

    #[test]
    fn round_trip() {
        let mut c = Canvas::new(3, 2);
        c.set_pixel(0, 0, Rgb888::new(255, 0, 0));
        c.set_pixel(2, 1, Rgb888::new(1, 2, 3));
        assert_eq!(decode(&encode(&c, 1, false)).unwrap(), c);
    }

    #[test]
    fn scales_and_grids() {
        let mut c = Canvas::new(1, 1);
        c.set_pixel(0, 0, Rgb888::new(160, 160, 160));
        let big = decode(&encode(&c, 4, true)).unwrap();
        assert_eq!((big.width(), big.height()), (4, 4));
        assert_eq!(big.pixel(0, 0), Some(Rgb888::new(160, 160, 160)));
        assert_eq!(big.pixel(3, 0), Some(Rgb888::new(20, 20, 20)));
    }
}
