use std::collections::HashMap;
use fontdue::Font;

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

/// A rasterized glyph: coverage bitmap + placement metrics, all in physical px.
struct Glyph {
    bitmap: Vec<u8>, // coverage, width*height bytes
    width: usize,
    height: usize,
    xmin: i32,     // offset from pen x to bitmap left
    ymin: i32,     // fontdue: offset of bitmap bottom from baseline
    advance: f32,  // pen advance in physical px
}

#[derive(Default)]
pub struct Layout {
    pub phys_width: i32,
    pub phys_height: i32,
    pub dest_width: i32,
    pub pad_x: i32,
    pub baseline: i32
}

pub struct Renderer {
    font: Font,
    font_px: f32,
    /// Glyph cache keyed by (char, font_px bits)
    cache: HashMap<char, Glyph>,
    layout: Layout
}

impl Renderer {
    pub fn new(font: Font) -> Self {
        let mut r = Renderer { font, cache: HashMap::new(), font_px: LOGICAL_FONT_PX as f32, layout: Layout::default() };
        r.rescale(120, 1920);
        r
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    pub fn rescale(&mut self, scale120: i32, logical_width: i32) {
        let phys_height = (LOGICAL_HEIGHT * scale120 + 60) / 120;
        let phys_width  = (logical_width  * scale120 + 60) / 120;

        self.font_px = LOGICAL_FONT_PX as f32 * phys_height as f32 / LOGICAL_HEIGHT as f32;

        let lm = self.font.horizontal_line_metrics(self.font_px).unwrap();
        let baseline = ((phys_height as f32 - (lm.ascent - lm.descent)) * 0.5 + lm.ascent).round() as i32;

        self.cache.clear();

        self.layout = Layout {
            phys_height,
            phys_width,
            dest_width: (phys_width * LOGICAL_HEIGHT + phys_height / 2) / phys_height,
            pad_x: (LOGICAL_PAD_X * phys_height + LOGICAL_HEIGHT / 2) / LOGICAL_HEIGHT,
            baseline
        };
    }

    fn glyph(&mut self, ch: char) -> &Glyph {
        self.cache.entry(ch).or_insert_with(|| {
            let (m, bitmap) = self.font.rasterize(ch, self.font_px);
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
    fn measure(&mut self, s: &str) -> f32 {
        s.chars().map(|c| self.glyph(c).advance).sum()
    }

    /// Paint `bar` into `buf` (len must be width*height, ARGB8888 physical px).
    pub fn draw(&mut self, bar: &Bar, buf: &mut [u32], width: i32) {
        let width = width as usize;
        let height = self.layout.phys_height as usize;
        assert_eq!(buf.len(), width * height, "buffer size mismatch");

        // Background fill.
        for px in buf.iter_mut() {
            *px = BG;
        }
        // Left side: tags as full-height squares, first one flush to the left edge.
        let side = self.layout.phys_height;   // square = bar height, exactly
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
            let tw = self.measure(&label);
            let tx = pen + (side as f32 - tw) * 0.5;
            self.draw_text(&label, tx, text, buf, width, height);

            pen += side as f32;   // squares touch; add a gap here if you want spacing
        }

        // Right side: assemble the status string, right-align it.
        let right = self.right_text(bar);
        let text_w = self.measure(&right);
        let start_x = (width as f32) - self.layout.pad_x as f32 - text_w;
        self.draw_text(&right, start_x, FG, buf, width, height);
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
        buf: &mut [u32],
        width: usize,
        height: usize,
    ) -> f32 {
        let mut pen = x;
        let baseline = self.layout.baseline;
        for ch in s.chars() {
            // Copy the glyph out of the cache to avoid borrowing self across the
            // blit loop.
            let g = self.glyph(ch);
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
