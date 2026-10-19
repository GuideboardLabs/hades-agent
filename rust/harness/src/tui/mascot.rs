//! Native port of the user-supplied HTML's true-color half-block skull and flame shader.
//! Sampling and animation use the HTML's 96-column coordinate space. No image decoder,
//! JavaScript engine, browser, or filesystem access is needed at runtime.
use std::sync::OnceLock;

use ratatui::{buffer::Buffer, layout::Rect, style::Color, widgets::Widget};

const SIZE: usize = 96;
const RGB: &[u8; SIZE * SIZE * 3] = include_bytes!("../../../../assets/tui-skull.rgb");
pub type Pixel = [u8; 3];

fn base(x: usize, y: usize) -> Pixel {
    let offset = (y * SIZE + x) * 3;
    [RGB[offset], RGB[offset + 1], RGB[offset + 2]]
}

fn is_flame(pixel: Pixel, y: usize) -> bool {
    let [r, g, b] = pixel.map(i32::from);
    (y as f64) < SIZE as f64 * 0.58 && b > 95 && b > r + 55 && g > r + 10
}

fn animated_mask() -> &'static [bool] {
    static MASK: OnceLock<Vec<bool>> = OnceLock::new();
    MASK.get_or_init(|| {
        (0..SIZE * SIZE)
            .map(|index| {
                let (x, y) = (index % SIZE, index / SIZE);
                if y as f64 >= SIZE as f64 * 0.60
                    || (y as f64 > SIZE as f64 * 0.43 && !is_flame(base(x, y), y))
                {
                    return false;
                }
                (y.saturating_sub(3)..=(y + 3).min(SIZE - 1)).any(|sy| {
                    (x.saturating_sub(3)..=(x + 3).min(SIZE - 1))
                        .any(|sx| is_flame(base(sx, sy), sy))
                })
            })
            .collect()
    })
}

/// Remove only dark pixels connected to the image boundary. Black eye sockets,
/// nose, and other enclosed details belong to the artwork and keep their contrast.
fn exterior_mask() -> &'static [bool] {
    static MASK: OnceLock<Vec<bool>> = OnceLock::new();
    MASK.get_or_init(|| {
        let mut mask = vec![false; SIZE * SIZE];
        let mut pending = Vec::new();
        for i in 0..SIZE {
            pending.extend([(i, 0), (i, SIZE - 1), (0, i), (SIZE - 1, i)]);
        }
        while let Some((x, y)) = pending.pop() {
            let index = y * SIZE + x;
            if mask[index] || base(x, y).into_iter().max().unwrap_or(0) > 24 {
                continue;
            }
            mask[index] = true;
            if x > 0 {
                pending.push((x - 1, y));
            }
            if x + 1 < SIZE {
                pending.push((x + 1, y));
            }
            if y > 0 {
                pending.push((x, y - 1));
            }
            if y + 1 < SIZE {
                pending.push((x, y + 1));
            }
        }
        mask
    })
}

fn themed_pixels(time: Option<f64>, background: Pixel) -> Vec<Pixel> {
    let mut image = pixels(time);
    for (i, pixel) in image.iter_mut().enumerate() {
        let vacated_flame = animated_mask()[i] && *pixel == [0, 0, 0];
        if (exterior_mask()[i] && pixel.iter().copied().max().unwrap_or(0) <= 24) || vacated_flame {
            *pixel = background;
        }
    }
    image
}

/// The HTML's flameTint, including its silhouette movement and protection of the face.
fn flame_tint(original: Pixel, x: usize, y: usize, t: f64) -> Pixel {
    let (xf, yf) = (x as f64, y as f64);
    let height = (1.0 - yf / (SIZE as f64 * 0.64)).max(0.0);
    let bend = (yf * 0.16 - t * 2.6 + xf * 0.035).sin() * 1.8
        + (yf * 0.34 - t * 4.5 + xf * 0.16).sin() * 0.9;
    let dx = bend * (0.45 + height * 0.85);
    let dy = (xf * 0.17 - t * 3.2).sin() * 1.05 + (xf * 0.31 - t * 5.1).sin() * 0.65;
    // JavaScript's Math.round rounds ties toward positive infinity.
    let sx = (xf - dx + 0.5).floor() as isize;
    let sy = (yf - dy + 0.5).floor() as isize;
    if sx < 0 || sy < 0 || sx >= SIZE as isize || sy >= SIZE as isize {
        return original;
    }
    let sampled = base(sx as usize, sy as usize);
    if !is_flame(sampled, sy as usize) {
        return if is_flame(original, y) {
            [0, 0, 0]
        } else {
            original
        };
    }
    if yf > SIZE as f64 * 0.43 && !is_flame(original, y) {
        return original;
    }
    let w = (xf * 0.23 - yf * 0.36 + t * 5.5).sin() * 0.5
        + (xf * 0.48 + yf * 0.12 - t * 8.0).sin() * 0.5;
    let [r, g, b] = sampled.map(f64::from);
    [
        r * (0.88 + 0.17 * w) + w.max(0.0) * 8.0,
        g * (0.93 + 0.12 * w) + 13.0,
        b * (0.95 + 0.07 * w) + 8.0,
    ]
    .map(|n| n.clamp(0.0, 255.0).round() as u8)
}

/// `None` is the exact unanimated source, including its original colors.
pub fn pixels(time: Option<f64>) -> Vec<Pixel> {
    (0..SIZE * SIZE)
        .map(|index| {
            let (x, y) = (index % SIZE, index / SIZE);
            let original = base(x, y);
            if let Some(t) = time.filter(|_| animated_mask()[index]) {
                flame_tint(original, x, y, t + if y % 2 == 1 { 0.05 } else { 0.0 })
            } else {
                original
            }
        })
        .collect()
}

fn sample(pixels: &[Pixel], x: usize, y: usize, width: usize) -> Pixel {
    let start_x = x * SIZE / width;
    let end_x = ((x + 1) * SIZE / width).max(start_x + 1).min(SIZE);
    let start_y = y * SIZE / width;
    let end_y = ((y + 1) * SIZE / width).max(start_y + 1).min(SIZE);
    let mut sum = [0u32; 3];
    let mut count = 0;
    for sy in start_y..end_y {
        for sx in start_x..end_x {
            for (channel, total) in sum.iter_mut().enumerate() {
                *total += pixels[sy * SIZE + sx][channel] as u32;
            }
            count += 1;
        }
    }
    sum.map(|n| (n / count) as u8)
}

pub struct Mascot {
    pub frame: u64,
    pub motion: bool,
}
impl Widget for Mascot {
    fn render(self, area: Rect, buffer: &mut Buffer) {
        let width = area
            .width
            .min(area.height.saturating_mul(2))
            .min(SIZE as u16)
            / 2
            * 2;
        if width < 2 {
            return;
        }
        let x = area.x + (area.width - width) / 2;
        let y = area.y + (area.height - width / 2) / 2;
        let time = self.motion.then_some(self.frame as f64 * 0.042);
        let image = match buffer[(x, y)].bg {
            Color::Rgb(r, g, b) => themed_pixels(time, [r, g, b]),
            _ => pixels(time),
        };
        for row in 0..width / 2 {
            for col in 0..width {
                let top = sample(&image, col as usize, row as usize * 2, width as usize);
                let bottom = sample(&image, col as usize, row as usize * 2 + 1, width as usize);
                buffer[(x + col, y + row)]
                    .set_symbol("▀")
                    .set_fg(Color::Rgb(top[0], top[1], top[2]))
                    .set_bg(Color::Rgb(bottom[0], bottom[1], bottom[2]));
            }
        }
    }
}
