//! String sinks: how facts get their [`Sym`]s.

use crate::intern::{Interner, Sym};

/// Hands out a [`Sym`] for a byte span of the source being extracted.
///
/// The extractor calls [`StrSink::begin_file`] once per file, then
/// [`StrSink::sym`] for every name it records. Swapping the sink swaps what the
/// `Sym` columns mean (span index vs interned id) without changing the tables.
pub trait StrSink {
    /// A new file starts; `src` is the (possibly truncated) source.
    fn begin_file(&mut self, src: &[u8]) {
        let _ = src;
    }

    /// The id for `src[start..start + len]`.
    fn sym(&mut self, src: &[u8], start: u32, len: u32) -> Sym;
}

/// Pre-interning sink: `Sym` index `i` is the `i`-th recorded span of the
/// current file. Cleared on every [`StrSink::begin_file`], so it describes the
/// last extracted file only; resolve with [`SpanStrs::text`] against that file's
/// source. 8 bytes per recorded name, transient (not part of the facts).
#[derive(Default, Debug)]
pub struct SpanStrs {
    spans: Vec<(u32, u32)>,
}

impl SpanStrs {
    pub fn new() -> SpanStrs {
        SpanStrs::default()
    }

    /// `(start, len)` byte span of `sym`.
    #[inline]
    pub fn span(&self, sym: Sym) -> (u32, u32) {
        self.spans[sym.index()]
    }

    /// The bytes of `sym` in `src` (the source of the last extracted file).
    #[inline]
    pub fn text<'s>(&self, src: &'s [u8], sym: Sym) -> &'s [u8] {
        let (s, l) = self.span(sym);
        &src[s as usize..(s + l) as usize]
    }

    /// Number of spans recorded for the current file.
    pub fn len(&self) -> usize {
        self.spans.len()
    }

    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    /// Heap bytes reserved.
    pub fn heap_bytes(&self) -> usize {
        self.spans.capacity() * 8
    }
}

impl StrSink for SpanStrs {
    #[inline]
    fn begin_file(&mut self, _src: &[u8]) {
        self.spans.clear();
    }

    #[inline]
    fn sym(&mut self, _src: &[u8], start: u32, len: u32) -> Sym {
        let i = self.spans.len() as u32;
        self.spans.push((start, len));
        Sym::from_index(i)
    }
}

/// Interning sink: ids are this interner's; the source may be dropped after extraction.
impl StrSink for Interner {
    #[inline]
    fn sym(&mut self, src: &[u8], start: u32, len: u32) -> Sym {
        self.intern(&src[start as usize..(start + len) as usize])
    }
}
