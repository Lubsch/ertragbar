use std::collections::HashMap;
use fontdue::Font;

use crate::Bar;
use crate::battery::{Battery, Percentage};
use crate::network::Network;
use crate::tags::TagState;

pub const LOGICAL_HEIGHT: f32 = 22.0;
const LOGICAL_FONT_PX: f32 = 14.0;
const LOGICAL_PAD_X: f32 = 5.0; // left/right inner padding

// Colors, ARGB8888 (0xAARRGGBB). Opaque background + foreground.
const BG: u32 = 0xff101010;
const FG: u32 = 0xffebdbb2;
const FG_DIM: u32 = 0xff101010; // empty tags

/// A rasterized glyph: coverage bitmap + placement metrics, all in physical px.
struct Glyph {
    bitmap: Vec<u8>, // coverage, width*height bytes
    width: usize,
    height: usize,
    xmin: i32,     // offset from pen x to bitmap left
    ymin: i32,     // fontdue: offset of bitmap bottom from baseline
    advance: f32,  // pen advance in physical px
}

/// Physical-pixel metrics derived from a scale factor.
#[derive(Clone, Copy)]
pub struct Metrics {
    pub height: u32,   // physical bar height
    pub font_px: f32,  // physical font size for fontdue
    pub pad_x: f32,
    pub baseline: f32, // physical y of the text baseline
}

impl Metrics {
    pub fn new(scale: f32) -> Self {
        let height = (LOGICAL_HEIGHT * scale).round() as u32;
        let scale = height as f32 / LOGICAL_HEIGHT; // effective scale, exact
        let font_px = LOGICAL_FONT_PX * scale;
        // Center text vertically: baseline ~ height*0.5 + font_px*0.35 is a
        // decent heuristic; refine with real font ascent if you want exactness.
        let baseline = (height as f32) * 0.5 + font_px * 0.35;
        Metrics {
            height,
            font_px,
            pad_x: LOGICAL_PAD_X * scale,
            baseline,
        }
    }
}

use crate::gamma::GammaLut;

pub struct Renderer {
    font: Font,
    /// Glyph cache keyed by (char, font_px bits)
    cache: HashMap<(char, u32), Glyph>,
    lut: GammaLut
}

impl Renderer {
    pub fn clear_cache(&mut self) {
        self.cache.clear();
    }

    pub fn new(font: Font) -> Self {
        Renderer { font, cache: HashMap::new(), lut: GammaLut::new() }
    }

    fn glyph(&mut self, ch: char, font_px: f32) -> &Glyph {
        let key = (ch, font_px.to_bits());
        self.cache.entry(key).or_insert_with(|| {
            let (m, bitmap) = self.font.rasterize(ch, font_px);
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

    /// Measure the advance width of a string in physical px (for right-aligning).
    fn measure(&mut self, s: &str, font_px: f32) -> f32 {
        s.chars().map(|c| self.glyph(c, font_px).advance).sum()
    }

    /// Paint `bar` into `buf` (len must be width*height, ARGB8888 physical px).
    pub fn draw(&mut self, bar: &Bar, m: &Metrics, buf: &mut [u32], width: u32) {
        let width = width as usize;
        let height = m.height as usize;
        assert_eq!(buf.len(), width * height, "buffer size mismatch");

        // Background fill.
        for px in buf.iter_mut() {
            *px = BG;
        }
        // Left side: tags as full-height squares, first one flush to the left edge.
        let side = m.height as i32;   // square = bar height, exactly
        let mut pen: f32 = 0.0;       // flush left, no pad

        for (i, tag) in bar.tags.iter().enumerate() {
            let label = format!("{}", i + 1);
            let (square, text) = match tag {
                TagState::Empty    => (BG, FG_DIM),
                TagState::Occupied => (BG, FG),
                TagState::Selected => (FG, BG),
            };

            let x0 = pen.round() as i32;
            fill_rect(buf, (width, height), (x0, 0), (side, side), square);

            // Center the digit inside the square.
            let tw = self.measure(&label, m.font_px);
            let tx = pen + (side as f32 - tw) * 0.5;
            self.draw_text(&label, tx, text, m, buf, width, height);

            pen += side as f32;   // squares touch; add a gap here if you want spacing
        }

        // Right side: assemble the status string, right-align it.
        let right = self.right_text(bar);
        let text_w = self.measure(&right, m.font_px);
        let start_x = (width as f32) - m.pad_x - text_w;
        self.draw_text(&right, start_x, FG, m, buf, width, height);
    }

    fn right_text(&self, bar: &Bar) -> String {
        let net = match bar.network {
            Network::WifiConnected => "Wifi",    // nf-fa-wifi
            Network::WifiDisconnected => "No Wifi :(", // nf-md-wifi_off
            Network::Ethernet => "Ethernet",         // nf-md-ethernet
        };
        let bat = match bar.battery {
            Battery::None => String::new(),
            Battery::Charging(Percentage(p)) => format!("{p}% ▲   "),
            Battery::Discharging(Percentage(p)) => format!("{p}% ▼   "),
            Battery::Unknown(Percentage(p)) => format!("{p}% —   ")
        };
        let crate::time::Time { year, month, day, hour, minute } = bar.time;
        format!("{net}   {bat}{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}")
    }

    /// Draw a run of text starting at pen x = `x`, baseline from `m`. Returns the
    /// pen x after the run.
    fn draw_text(
        &mut self,
        s: &str,
        x: f32,
        color: u32,
        m: &Metrics,
        buf: &mut [u32],
        width: usize,
        height: usize,
    ) -> f32 {
        let mut pen = x;
        let baseline = m.baseline;
        for ch in s.chars() {
            // Copy the glyph out of the cache to avoid borrowing self across the
            // blit loop.
            let g = self.glyph(ch, m.font_px);
            let (gw, gh, gxmin, gymin) = (g.width, g.height, g.xmin, g.ymin);
            let advance = g.advance;
            let bitmap = g.bitmap.clone();

            // Top-left corner of the glyph bitmap in buffer coords.
            let gx0 = (pen + gxmin as f32).round() as i32;
            // fontdue's ymin is the offset of the bitmap's *bottom* above baseline;
            // top = baseline - (ymin + height).
            let gy0 = (baseline - (gymin as f32 + gh as f32)).round() as i32;

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
                    let cov = bitmap[row * gw + col];
                    if cov == 0 {
                        continue;
                    }
                    let idx = py as usize * width + px as usize;
                    buf[idx] = self.lut.blend(buf[idx], color, cov);
                }
            }
            pen += advance;
        }
        pen
    }
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
