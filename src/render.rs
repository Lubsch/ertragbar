use fontdue::Font;
use std::collections::HashMap;

use crate::Bar;
use crate::battery::{Battery, Percentage};
use crate::network::Network;
use crate::tags::TagState;

pub const LOGICAL_HEIGHT: i32 = 22;
const LOGICAL_FONT_PX: i32 = 14;
const LOGICAL_PAD_X: i32 = 5; // left/right inner padding

// Colors, ARGB8888 (0xAARRGGBB). Opaque background + foreground.
const BG: u32 = 0xff101010;
const FG: u32 = 0xffebdbb2;
const FG_DIM: u32 = 0xff101010; // empty tags
const URGENT: u32 = 0xffcc241d; // urgent tags, low battery
const SEPARATOR: &str = "   ";

/// A rasterized glyph: coverage bitmap + placement metrics, all in physical px.
#[derive(Debug)]
struct Glyph {
    bitmap: Vec<u8>, // coverage, width*height bytes
    width: usize,
    height: usize,
    xmin: i32,    // offset from pen x to bitmap left
    ymin: i32,    // fontdue: offset of bitmap bottom from baseline
    advance: f32, // pen advance in physical px
}

/// Physical-pixel geometry of one bar, derived from its output's scale and
/// logical width by [`GlyphCache::layout`].
#[derive(Debug, Clone, Copy)]
pub struct Layout {
    pub phys_width: i32,
    pub phys_height: i32,
    pub dest_width: i32,
    pub pad_x: i32,
    pub baseline: i32,
    pub font_px: f32,
}

/// Paint `bar` into `buf` (len must be width*height, ARGB8888 physical px).
pub fn draw(
    buf: &mut [u32],
    bar: &Bar,
    tags: &[TagState; 9],
    layout: &Layout,
    gc: &mut GlyphCache,
) {
    let width = layout.phys_width as usize;
    let height = layout.phys_height as usize;
    assert_eq!(buf.len(), width * height, "buffer size mismatch");

    // Background fill.
    buf.fill(BG);

    // Left side: tags as full-height squares, first one flush to the left edge.
    let side = height as i32;
    let mut pen: f32 = 0.0; // flush left, no pad

    for (i, tag) in tags.iter().enumerate() {
        let label = format!("{}", i + 1);
        let (square, text) = match tag {
            TagState::Empty => (BG, FG_DIM),
            TagState::Occupied => (BG, FG),
            TagState::Urgent => (URGENT, FG),
            TagState::Selected => (FG, BG),
        };

        let x0 = pen.round() as i32;
        fill_rect(buf, (width, height), (x0, 0), (side, side), square);

        // Center the digit inside the square.
        let tw = gc.measure(&label, layout);
        let tx = pen + (side as f32 - tw) * 0.5;
        gc.draw_text(&label, tx, text, buf, width, height, layout);

        pen += side as f32; // squares touch; add a gap here if you want spacing
    }

    // Right side: status segments, right-aligned, separated by SEPARATOR.
    let segments = right_segments(bar);
    let sep_w = gc.measure(SEPARATOR, layout);
    let widths: Vec<f32> = segments
        .iter()
        .map(|(t, _)| gc.measure(t, layout))
        .collect();
    let total = widths.iter().sum::<f32>() + sep_w * (segments.len() - 1) as f32;

    let mut pen = (width as f32) - layout.pad_x as f32 - total;
    for ((text, highlight), w) in segments.iter().zip(widths) {
        if *highlight {
            // Full-height block, padded like the bar's edge padding.
            let x0 = pen.round() as i32 - layout.pad_x;
            let block_w = w.round() as i32 + 2 * layout.pad_x;
            fill_rect(buf, (width, height), (x0, 0), (block_w, side), URGENT);
        }
        gc.draw_text(text, pen, FG, buf, width, height, layout);
        pen += w + sep_w;
    }
}

/// A subset of Adwaita Sans with only the glyphs the bar can draw, built by
/// the flake (`packages.<system>.font`). Embedded, so there's nothing to load
/// at runtime, and small enough that fontdue parses it in well under 1ms.
const FONT: &[u8] = include_bytes!(env!("ERTRAGBAR_FONT"));

pub struct GlyphCache {
    font: Font,
    cache: HashMap<(char, u32), Glyph>,
}

impl GlyphCache {
    pub fn new() -> Self {
        GlyphCache {
            font: Font::from_bytes(FONT, fontdue::FontSettings::default())
                .expect("embedded font parses"),
            cache: HashMap::new(),
        }
    }

    /// Compute the physical layout for a bar `logical_width` wide at fractional
    /// scale `scale120` (120 == 1.0).
    pub fn layout(&mut self, scale120: u32, logical_width: u32) -> Layout {
        let scale120 = scale120 as i32;
        let phys_height = (LOGICAL_HEIGHT * scale120 + 60) / 120;
        let phys_width = (logical_width as i32 * scale120 + 60) / 120;
        let font_px = LOGICAL_FONT_PX as f32 * phys_height as f32 / LOGICAL_HEIGHT as f32;

        let lm = self
            .font
            .horizontal_line_metrics(font_px)
            .expect("font has horizontal metrics");
        let baseline =
            ((phys_height as f32 - (lm.ascent - lm.descent)) * 0.5 + lm.ascent).round() as i32;

        Layout {
            phys_width,
            phys_height,
            dest_width: (phys_width * LOGICAL_HEIGHT + phys_height / 2) / phys_height,
            pad_x: (LOGICAL_PAD_X * phys_height + LOGICAL_HEIGHT / 2) / LOGICAL_HEIGHT,
            baseline,
            font_px,
        }
    }

    /// Measure the advance width of a string in physical px (for right-aligning).
    fn measure(&mut self, s: &str, layout: &Layout) -> f32 {
        s.chars().map(|c| self.glyph(c, layout).advance).sum()
    }

    fn glyph(&mut self, ch: char, layout: &Layout) -> &Glyph {
        let font = &self.font;
        // Keyed by exact size so outputs with different scales share one cache.
        self.cache
            .entry((ch, layout.font_px.to_bits()))
            .or_insert_with(|| {
                let (m, bitmap) = font.rasterize(ch, layout.font_px);
                Glyph {
                    bitmap,
                    width: m.width,
                    height: m.height,
                    xmin: m.xmin,
                    ymin: m.ymin,
                    advance: m.advance_width,
                }
            })
    }

    /// Returns the pen x after the run.
    fn draw_text(
        &mut self,
        s: &str,
        x: f32,
        color: u32,
        buf: &mut [u32],
        width: usize,
        height: usize,
        layout: &Layout,
    ) -> f32 {
        let mut pen = x;
        let baseline = layout.baseline;
        for ch in s.chars() {
            // Copy the glyph out of the cache to avoid borrowing self across the
            // blit loop.
            let g = self.glyph(ch, layout);
            let (gw, gh, gxmin, gymin) = (g.width, g.height, g.xmin, g.ymin);
            let advance = g.advance;
            let bitmap = &g.bitmap;

            // Top-left corner of the glyph bitmap in buffer coords.
            let gx0 = (pen + gxmin as f32).round() as i32;
            // fontdue's ymin is the offset of the bitmap's *bottom* above baseline;
            // top = baseline - (ymin + height).
            let gy0 = baseline - gymin - gh as i32;

            for row in 0..gh {
                let py = gy0 + row as i32;
                if py < 0 || py >= height as i32 {
                    continue;
                }
                for col in 0..gw {
                    let px = gx0 + col as i32;
                    if px < 0 || px >= width as i32 {
                        continue;
                    }
                    let cov = bitmap[row * gw + col] as u32;
                    if cov == 0 {
                        continue;
                    }
                    let idx = py as usize * width + px as usize;
                    let bg = buf[idx];
                    if cov == 255 {
                        buf[idx] = color;
                        continue;
                    }
                    let inv = 255 - cov;
                    let ch = |shift: u32| -> u32 {
                        let fg_c = (color >> shift) & 0xff;
                        let bg_c = (bg >> shift) & 0xff;
                        (fg_c * cov + bg_c * inv + 127) / 255
                    };
                    buf[idx] = 0xff00_0000 | (ch(16) << 16) | (ch(8) << 8) | ch(0);
                }
            }
            pen += advance;
        }
        pen
    }
}

/// Status text, left to right, each with whether it gets the alert highlight.
fn right_segments(bar: &Bar) -> Vec<(String, bool)> {
    let net = match bar.network {
        Network::WifiConnected => "Wifi",
        Network::WifiDisconnected => "No Wifi :(",
        Network::Ethernet => "Ethernet",
    };
    let mut segments = vec![(net.to_owned(), false)];

    let bat = match bar.battery {
        Battery::None => None,
        Battery::Charging(Percentage(p)) => Some(format!("{p}% ▲")),
        Battery::Discharging(Percentage(p)) => Some(format!("{p}% ▼")),
        Battery::Unknown(Percentage(p)) => Some(format!("{p}% —")),
    };
    if let Some(bat) = bat {
        segments.push((bat, bar.battery.is_low()));
    }

    let crate::time::Time {
        year,
        month,
        day,
        hour,
        minute,
    } = bar.time;
    segments.push((
        format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}"),
        false,
    ));
    segments
}

fn fill_rect(
    buf: &mut [u32],
    (width, height): (usize, usize),
    (x, y): (i32, i32),
    (w, h): (i32, i32),
    color: u32,
) {
    for row in 0..h {
        let py = y + row;
        if py < 0 || py >= height as i32 {
            continue;
        }
        let base = py as usize * width;
        for col in 0..w {
            let px = x + col;
            if px < 0 || px >= width as i32 {
                continue;
            }
            buf[base + px as usize] = color;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::Time;

    fn render(battery: Battery) -> Vec<u32> {
        let mut gc = GlyphCache::new();
        let layout = gc.layout(120, 400);
        let bar = bar(Network::WifiConnected, battery);
        let mut buf = vec![0; (layout.phys_width * layout.phys_height) as usize];
        draw(&mut buf, &bar, &[TagState::Empty; 9], &layout, &mut gc);
        buf
    }

    fn bar(network: Network, battery: Battery) -> Bar {
        let time = Time {
            year: 2026,
            month: 1,
            day: 1,
            hour: 0,
            minute: 0,
        };
        Bar {
            network,
            battery,
            time,
        }
    }

    /// The font is a subset; a character missing from it would render as
    /// nothing. Check everything the bar can print is in there.
    #[test]
    fn font_covers_all_text() {
        let gc = GlyphCache::new();
        let p = Percentage(100);
        let batteries = [
            Battery::Charging(p),
            Battery::Discharging(p),
            Battery::Unknown(p),
        ];
        let networks = [
            Network::WifiConnected,
            Network::WifiDisconnected,
            Network::Ethernet,
        ];
        let mut text = String::from("0123456789"); // tag labels, all digits
        text += SEPARATOR;
        for n in networks {
            for b in batteries {
                for (s, _) in right_segments(&bar(n, b)) {
                    text += &s;
                }
            }
        }
        let missing: String = text
            .chars()
            .filter(|&c| gc.font.lookup_glyph_index(c) == 0)
            .collect();
        assert!(missing.is_empty(), "not in font subset: {missing:?}");
    }

    #[test]
    fn low_battery_is_highlighted() {
        assert!(render(Battery::Discharging(Percentage(10))).contains(&URGENT));
        assert!(!render(Battery::Discharging(Percentage(50))).contains(&URGENT));
    }
}
