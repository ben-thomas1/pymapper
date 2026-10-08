//! Label text: font loading, glyph rasterisation into an own alpha atlas, and label layout
//! into world-anchored [`GlyphInstance`]s.
//!
//! Fonts and rasterisation come from glyphon's cosmic-text (swash). Only egui's built-in
//! fonts are loaded (no system font scan: fast startup, small memory). Layout is a simple
//! advance-based pen walk with 4 horizontal subpixel bins (no shaping): labels are short
//! identifiers, and this keeps a full label rebuild in the low milliseconds. ASCII glyphs
//! are looked up in a flat table; other characters in a hash map.
//!
//! Glyph positions are physical-pixel offsets from a label origin that the vertex shader
//! puts at the labelled rect's top-left, so text stays crisp at any zoom and costs no CPU
//! work when the camera moves.

use std::collections::HashMap;
use std::sync::Arc;

use glyphon::{CacheKey, FontSystem, SwashCache, SwashContent, Weight, cosmic_text::CacheKeyFlags, fontdb};

use super::scene::{GlyphInstance, Rgba, WorldRect};

/// Subpixel bins per pixel along x.
const BINS: usize = 4;
/// Gap between atlas entries.
const PAD: u32 = 1;
/// A label is shown only where at least this many of its characters fit.
pub const MIN_LABEL_CHARS: usize = 4;

/// One rasterised glyph in the atlas (or an empty one, e.g. a space).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct GlyphEntry {
    pub uv: [u16; 2],
    pub size: [u8; 2],
    /// Bitmap top-left relative to the pen position on the baseline (y down).
    pub offset: [i16; 2],
    pub advance: f32,
}

/// Shelf packer over a square-ish atlas.
#[derive(Debug, Clone)]
pub struct ShelfPacker {
    pub width: u32,
    pub height: u32,
    /// (y, height, next free x) per shelf.
    shelves: Vec<(u32, u32, u32)>,
    next_y: u32,
}

impl ShelfPacker {
    pub fn new(width: u32, height: u32) -> Self {
        Self { width, height, shelves: Vec::new(), next_y: 0 }
    }

    /// Allocates a `w` x `h` region, or `None` when full.
    pub fn alloc(&mut self, w: u32, h: u32) -> Option<[u32; 2]> {
        let (pw, ph) = (w + PAD, h + PAD);
        if pw > self.width {
            return None;
        }
        // Best fit: the lowest shelf that is tall enough and not wastefully taller.
        let mut best: Option<usize> = None;
        for (i, s) in self.shelves.iter().enumerate() {
            if s.1 >= ph && s.1 <= ph + ph / 2 + 2 && s.2 + pw <= self.width && best.is_none_or(|b| s.1 < self.shelves[b].1) {
                best = Some(i);
            }
        }
        if let Some(i) = best {
            let s = &mut self.shelves[i];
            let at = [s.2, s.0];
            s.2 += pw;
            return Some(at);
        }
        if self.next_y + ph > self.height {
            return None;
        }
        let y = self.next_y;
        self.next_y += ph;
        self.shelves.push((y, ph, pw));
        Some([0, y])
    }

    pub fn clear(&mut self) {
        self.shelves.clear();
        self.next_y = 0;
    }
}

/// A glyph bitmap waiting to be copied into the atlas texture.
#[derive(Debug, Clone, Copy)]
pub struct PendingGlyph {
    pub at: [u32; 2],
    pub size: [u32; 2],
    /// Byte offset into [`TextSystem::pending_pixels`].
    pub offset: usize,
}

/// Fonts, glyph cache and atlas allocation (the CPU side of the glyph atlas).
pub struct TextSystem {
    fonts: FontSystem,
    swash: SwashCache,
    /// Fallback chain (face ids), first match wins.
    faces: Vec<fontdb::ID>,
    /// Font size in physical pixels.
    px: f32,
    ascent: f32,
    line_h: f32,
    ascii: Vec<Option<GlyphEntry>>,
    other: HashMap<(char, u8), GlyphEntry>,
    pub packer: ShelfPacker,
    pub pending: Vec<PendingGlyph>,
    pub pending_pixels: Vec<u8>,
    /// Set when the atlas ran out of space; the renderer grows it and lays out again.
    pub atlas_full: bool,
}

impl std::fmt::Debug for TextSystem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TextSystem").field("px", &self.px).field("faces", &self.faces.len()).finish_non_exhaustive()
    }
}

/// egui's proportional font chain (bytes are compiled into egui).
fn default_faces(db: &mut fontdb::Database) -> Vec<fontdb::ID> {
    let defs = egui::FontDefinitions::default();
    let mut ids = Vec::new();
    let Some(chain) = defs.families.get(&egui::FontFamily::Proportional) else { return ids };
    for name in chain {
        let Some(data) = defs.font_data.get(name) else { continue };
        let source: Arc<dyn AsRef<[u8]> + Send + Sync> = match &data.font {
            std::borrow::Cow::Borrowed(b) => Arc::new(*b),
            std::borrow::Cow::Owned(v) => Arc::new(v.clone()),
        };
        let face = db.load_font_source(fontdb::Source::Binary(source));
        if let Some(&id) = face.get(data.index as usize).or(face.first()) {
            ids.push(id);
        }
    }
    ids
}

impl TextSystem {
    /// Text at `px` physical pixels, with an atlas of `atlas` x `atlas` texels.
    pub fn new(px: f32, atlas: u32) -> Self {
        let mut db = fontdb::Database::new();
        let faces = default_faces(&mut db);
        let fonts = FontSystem::new_with_locale_and_db("en-US".to_owned(), db);
        let mut t = Self {
            fonts,
            swash: SwashCache::new(),
            faces,
            px: 0.0,
            ascent: 0.0,
            line_h: 0.0,
            ascii: vec![None; 128 * BINS],
            other: HashMap::new(),
            packer: ShelfPacker::new(atlas, atlas),
            pending: Vec::new(),
            pending_pixels: Vec::new(),
            atlas_full: false,
        };
        t.set_px(px);
        t
    }

    pub fn px(&self) -> f32 {
        self.px
    }
    /// Distance from the label top to the baseline, in pixels.
    pub fn ascent(&self) -> f32 {
        self.ascent
    }
    pub fn line_height(&self) -> f32 {
        self.line_h
    }

    /// Changes the font size; drops the glyph cache and atlas contents.
    pub fn set_px(&mut self, px: f32) {
        // Glyph bitmaps must fit in u8 sizes (GlyphInstance::size).
        let px = if px.is_finite() { px.clamp(4.0, 160.0) } else { 12.0 };
        if px == self.px {
            return;
        }
        self.px = px;
        let (mut ascent, mut descent) = (px * 0.8, px * 0.2);
        if let Some(&id) = self.faces.first()
            && let Some(font) = self.fonts.get_font(id, Weight::NORMAL)
        {
            let m = font.as_swash().metrics(&[]).scale(px);
            ascent = m.ascent;
            descent = m.descent;
        }
        self.ascent = ascent.ceil();
        self.line_h = (ascent + descent).ceil();
        self.reset_atlas(self.packer.width, self.packer.height);
    }

    /// Forgets every cached glyph and starts a fresh atlas of the given size.
    pub fn reset_atlas(&mut self, width: u32, height: u32) {
        self.ascii.iter_mut().for_each(|g| *g = None);
        self.other.clear();
        self.packer = ShelfPacker::new(width, height);
        self.pending.clear();
        self.pending_pixels.clear();
        self.atlas_full = false;
    }

    /// The glyph for `ch` at subpixel bin `bin`, rasterising it on first use.
    pub fn glyph(&mut self, ch: char, bin: u8) -> GlyphEntry {
        let bin = bin.min(BINS as u8 - 1);
        let ai = (ch as usize) * BINS + bin as usize;
        if (ch as u32) < 128
            && let Some(g) = self.ascii[ai]
        {
            return g;
        }
        if (ch as u32) >= 128
            && let Some(g) = self.other.get(&(ch, bin))
        {
            return *g;
        }
        let g = self.rasterise(ch, bin);
        if (ch as u32) < 128 {
            self.ascii[ai] = Some(g);
        } else {
            self.other.insert((ch, bin), g);
        }
        g
    }

    fn rasterise(&mut self, ch: char, bin: u8) -> GlyphEntry {
        let mut found = None;
        for &id in &self.faces {
            let Some(font) = self.fonts.get_font(id, Weight::NORMAL) else { continue };
            let gid = font.as_swash().charmap().map(ch);
            if gid != 0 {
                let adv = font.as_swash().glyph_metrics(&[]).scale(self.px).advance_width(gid);
                found = Some((id, gid, adv));
                break;
            }
        }
        // Missing everywhere: use the first face's .notdef box.
        let (id, gid, advance) = match (found, self.faces.first()) {
            (Some(f), _) => f,
            (None, Some(&id)) => (id, 0, self.px * 0.5),
            (None, None) => return GlyphEntry { advance: self.px * 0.5, ..GlyphEntry::default() },
        };
        let mut entry = GlyphEntry { advance, ..GlyphEntry::default() };
        let x = f32::from(bin) / BINS as f32;
        let (key, _, _) = CacheKey::new(id, gid, self.px, (x, 0.0), Weight::NORMAL, CacheKeyFlags::empty());
        let Some(img) = self.swash.get_image_uncached(&mut self.fonts, key) else { return entry };
        let (w, h) = (img.placement.width, img.placement.height);
        if w == 0 || h == 0 || w > u32::from(u8::MAX) || h > u32::from(u8::MAX) {
            return entry;
        }
        let Some(at) = self.packer.alloc(w, h) else {
            self.atlas_full = true;
            return entry;
        };
        let offset = self.pending_pixels.len();
        match img.content {
            SwashContent::Mask => self.pending_pixels.extend_from_slice(&img.data[..(w * h) as usize]),
            // Colour bitmaps (emoji) and subpixel masks: keep coverage only.
            SwashContent::Color | SwashContent::SubpixelMask => {
                self.pending_pixels.extend(img.data.as_chunks::<4>().0.iter().take((w * h) as usize).map(|p| p[3]));
            }
        }
        self.pending.push(PendingGlyph { at, size: [w, h], offset });
        entry.uv = [at[0] as u16, at[1] as u16];
        entry.size = [w as u8, h as u8];
        entry.offset = [img.placement.left.clamp(-32768, 32767) as i16, (-img.placement.top).clamp(-32768, 32767) as i16];
        entry
    }

    /// Appends the glyphs of `text` for a label on `rect`, stopping once the pen passes
    /// `max_px`. Returns the label width in pixels. The label shows only where its first
    /// [`MIN_LABEL_CHARS`] characters (or all of a shorter label) fit.
    pub fn layout(&mut self, text: &[u8], rect: WorldRect, color: Rgba, max_px: f32, out: &mut Vec<GlyphInstance>) -> f32 {
        let first = out.len();
        let mut pen = 0.0f32;
        let mut need = f32::INFINITY;
        for (n, ch) in String::from_utf8_lossy(text).chars().enumerate() {
            if n == MIN_LABEL_CHARS {
                need = pen;
            }
            if pen > max_px {
                break;
            }
            if ch.is_control() {
                continue;
            }
            let ix = pen.floor();
            let bin = (((pen - ix) * BINS as f32) as u8).min(BINS as u8 - 1);
            let g = self.glyph(ch, bin);
            if g.size[0] > 0 {
                out.push(GlyphInstance {
                    rect_min: rect.min,
                    rect_max: rect.max,
                    offset: [(ix as i32 + i32::from(g.offset[0])).clamp(-32768, 32767) as i16, g.offset[1]],
                    size: g.size,
                    need_px: 0,
                    uv: g.uv,
                    color,
                });
            }
            pen += g.advance;
        }
        let need = need.min(pen).ceil().clamp(0.0, f32::from(u16::MAX)) as u16;
        for g in &mut out[first..] {
            g.need_px = need;
        }
        pen
    }
}
