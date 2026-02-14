use std::ops::Range;
use std::sync::Arc;

use bytemuck::TransparentWrapper;
use krilla::geom::PathBuilder;
use krilla::surface::{Location, Surface};
use krilla::text::GlyphId;
use ttf_parser::OutlineBuilder;
use typst_library::diag::{SourceResult, bail};
use typst_library::layout::Size;
use typst_library::text::{Font, Glyph, TextItem};
use typst_library::visualize::FillRule;
use typst_syntax::Span;
use typst_utils::defer;

use crate::convert::{FrameContext, GlobalContext};
use crate::util::{AbsExt, TransformExt, display_font};
use crate::{paint, tags};

#[typst_macros::time(name = "handle text")]
pub(crate) fn handle_text(
    fc: &mut FrameContext,
    t: &TextItem,
    surface: &mut Surface,
    gc: &mut GlobalContext,
) -> SourceResult<()> {
    let mut handle = tags::text(gc, fc, surface, t);
    let surface = handle.surface();

    let fill = paint::convert_fill(
        gc,
        &t.fill,
        FillRule::NonZero,
        true,
        surface,
        fc.state(),
        Size::zero(),
    )?;
    let stroke =
        if let Some(stroke) = t.stroke.as_ref().map(|s| {
            paint::convert_stroke(gc, s, true, surface, fc.state(), Size::zero())
        }) {
            Some(stroke?)
        } else {
            None
        };

    surface.push_transform(&fc.state().transform().to_krilla());
    let mut surface = defer(surface, |s| s.pop());
    surface.set_fill(Some(fill));
    surface.set_stroke(stroke);

    if gc.options.embed_fonts {
        let font = convert_font(gc, t.font.clone())?;
        let text = t.text.as_str();
        let size = t.size;
        let glyphs: &[PdfGlyph] =
            TransparentWrapper::wrap_slice(t.glyphs.as_slice());

        surface.draw_glyphs(
            krilla::geom::Point::from_xy(0.0, 0.0),
            glyphs,
            font.clone(),
            text,
            size.to_f32(),
            false,
        );
    } else {
        draw_glyphs_as_outlines(&mut surface, t)?;
    }

    Ok(())
}

/// Draws text glyphs as outlined vector paths instead of embedding font data.
/// Each glyph outline is extracted from the font's glyph tables and rendered
/// as a filled path. This avoids embedding the font in the PDF but makes
/// the text non-selectable and non-searchable.
fn draw_glyphs_as_outlines(
    surface: &mut Surface,
    t: &TextItem,
) -> SourceResult<()> {
    let font = &t.font;
    let size = t.size.to_f32();
    let upem = font.units_per_em() as f32;
    let scale = size / upem;

    let mut x_offset = 0.0f32;

    for glyph in t.glyphs.iter() {
        let glyph_id = ttf_parser::GlyphId(glyph.id);
        let dx = x_offset + glyph.x_offset.get() as f32 * size;
        let dy = glyph.y_offset.get() as f32 * size;

        let mut builder = KrillaOutlineBuilder::new();
        let has_outline = font.ttf().outline_glyph(glyph_id, &mut builder);

        if has_outline.is_some() {
            if let Some(path) = builder.finish() {
                // Font coordinates are Y-up; PDF is Y-down, so flip Y.
                // Apply scale and translation for glyph position.
                let transform = krilla::geom::Transform::from_row(
                    scale, 0.0, 0.0, -scale, dx, dy,
                );
                if let Some(path) = path.transform(transform) {
                    surface.draw_path(&path);
                }
            }
        }

        x_offset += glyph.x_advance.get() as f32 * size;
    }

    Ok(())
}

/// An adapter that implements `ttf_parser::OutlineBuilder` by forwarding
/// path commands to krilla's `PathBuilder`.
struct KrillaOutlineBuilder {
    builder: PathBuilder,
}

impl KrillaOutlineBuilder {
    fn new() -> Self {
        Self { builder: PathBuilder::new() }
    }

    fn finish(self) -> Option<krilla::geom::Path> {
        self.builder.finish()
    }
}

impl OutlineBuilder for KrillaOutlineBuilder {
    fn move_to(&mut self, x: f32, y: f32) {
        self.builder.move_to(x, y);
    }

    fn line_to(&mut self, x: f32, y: f32) {
        self.builder.line_to(x, y);
    }

    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        self.builder.quad_to(x1, y1, x, y);
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        self.builder.cubic_to(x1, y1, x2, y2, x, y);
    }

    fn close(&mut self) {
        self.builder.close();
    }
}

fn convert_font(
    gc: &mut GlobalContext,
    typst_font: Font,
) -> SourceResult<krilla::text::Font> {
    if let Some(font) = gc.fonts_forward.get(&typst_font) {
        Ok(font.clone())
    } else {
        let font = build_font(typst_font.clone())?;

        gc.fonts_forward.insert(typst_font.clone(), font.clone());
        gc.fonts_backward.insert(font.clone(), typst_font.clone());

        Ok(font)
    }
}

#[comemo::memoize]
fn build_font(typst_font: Font) -> SourceResult<krilla::text::Font> {
    let font_data: Arc<dyn AsRef<[u8]> + Send + Sync> =
        Arc::new(typst_font.data().clone());

    match krilla::text::Font::new(font_data.into(), typst_font.index()) {
        Some(f) => Ok(f),
        None => {
            bail!(
                Span::detached(),
                "failed to process {}",
                display_font(Some(&typst_font)),
            )
        }
    }
}

#[derive(Debug, TransparentWrapper)]
#[repr(transparent)]
struct PdfGlyph(Glyph);

impl krilla::text::Glyph for PdfGlyph {
    #[inline(always)]
    fn glyph_id(&self) -> GlyphId {
        GlyphId::new(self.0.id as u32)
    }

    #[inline(always)]
    fn text_range(&self) -> Range<usize> {
        self.0.range.start as usize..self.0.range.end as usize
    }

    #[inline(always)]
    fn x_advance(&self, size: f32) -> f32 {
        // Don't use `Em::at`, because it contains an expensive check whether the result is finite.
        self.0.x_advance.get() as f32 * size
    }

    #[inline(always)]
    fn x_offset(&self, size: f32) -> f32 {
        // Don't use `Em::at`, because it contains an expensive check whether the result is finite.
        self.0.x_offset.get() as f32 * size
    }

    #[inline(always)]
    fn y_offset(&self, size: f32) -> f32 {
        // Don't use `Em::at`, because it contains an expensive check whether the result is finite.
        self.0.y_offset.get() as f32 * size
    }

    #[inline(always)]
    fn y_advance(&self, size: f32) -> f32 {
        // Don't use `Em::at`, because it contains an expensive check whether the result is finite.
        self.0.y_advance.get() as f32 * size
    }

    fn location(&self) -> Option<Location> {
        Some(self.0.span.0.into_raw())
    }
}
