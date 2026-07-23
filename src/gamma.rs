//! Gamma-correct alpha blending.
//!
//! Anti-aliased text must be composited in *linear* light, not directly on
//! sRGB-encoded values, or edges come out too thin/light. We precompute:
//!   - SRGB_TO_LIN: 256 sRGB bytes -> linear f32
//!   - LIN_TO_SRGB: a 4096-entry linear -> sRGB byte table
//!
//! so the hot path does two lookups + a lerp per channel, no powf.
//! Build the tables once at startup with `GammaLut::new()` and pass a reference
//! into `draw`, or make it a lazy static. They're ~16 KB + 4 KB, cheap.

pub struct GammaLut {
    srgb_to_lin: [f32; 256],
    lin_to_srgb: [u8; Self::LIN_STEPS],
}

impl GammaLut {
    const LIN_STEPS: usize = 4096;

    pub fn new() -> Self {
        let mut srgb_to_lin = [0.0f32; 256];
        for (i, e) in srgb_to_lin.iter_mut().enumerate() {
            let c = i as f32 / 255.0;
            *e = if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            };
        }
        let mut lin_to_srgb = [0u8; Self::LIN_STEPS];
        for (i, e) in lin_to_srgb.iter_mut().enumerate() {
            let l = i as f32 / (Self::LIN_STEPS - 1) as f32;
            let s = if l <= 0.0031308 {
                12.92 * l
            } else {
                1.055 * l.powf(1.0 / 2.4) - 0.055
            };
            *e = (s * 255.0 + 0.5).clamp(0.0, 255.0) as u8;
        }
        GammaLut { srgb_to_lin, lin_to_srgb }
    }

    #[inline]
    fn encode(&self, linear: f32) -> u32 {
        let idx = (linear * (Self::LIN_STEPS - 1) as f32 + 0.5) as usize;
        self.lin_to_srgb[idx.min(Self::LIN_STEPS - 1)] as u32
    }

    /// Blend `fg` over `bg` with coverage `cov` (0..=255), in linear light.
    /// Both are ARGB8888 with opaque alpha; output stays opaque.
    #[inline]
    pub fn blend(&self, bg: u32, fg: u32, cov: u8) -> u32 {
        if cov == 0 {
            return bg;
        }
        if cov == 255 {
            return fg;
        }
        let a = cov as f32 / 255.0;
        let ch = |shift: u32| -> u32 {
            let bl = self.srgb_to_lin[((bg >> shift) & 0xff) as usize];
            let fl = self.srgb_to_lin[((fg >> shift) & 0xff) as usize];
            self.encode(fl * a + bl * (1.0 - a))
        };
        0xff00_0000 | (ch(16) << 16) | (ch(8) << 8) | ch(0)
    }
}

impl Default for GammaLut {
    fn default() -> Self {
        Self::new()
    }
}
