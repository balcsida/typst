# Plan: Add Font Embedding Control to Krilla

## Context

Krilla is the PDF backend library used by Typst. Currently, all fonts used in a
document are always embedded (subsetted) into the output PDF. This plan describes
how to add a `no_embed_fonts` option to `SerializeSettings` so that consumers
like Typst can produce PDFs without embedded font programs, which is needed for
font license compliance (see typst/typst#6466).

## Repository

- https://github.com/LaurenzV/krilla
- Working commit: `84c6332`

## Goal

Add a `no_embed_fonts: bool` field to `SerializeSettings`. When `true`, the PDF
is generated without font program streams (`/FontFile2`, `/FontFile3`). Font
descriptors, widths, Unicode CMaps, and text operators (`Tj`/`TJ`) are kept so
that text remains selectable and searchable, but viewers must substitute fonts
locally. This is a standard, well-supported PDF pattern.

---

## Step 1 -- Add the setting

**File:** `crates/krilla/src/serialize.rs`

Add the field to `SerializeSettings`:

```rust
pub struct SerializeSettings {
    // ... existing fields ...

    /// When `true`, font programs (the binary font data) will **not** be
    /// embedded in the PDF.  Font descriptors, metrics, and Unicode CMaps
    /// are still written so that text remains selectable.  PDF viewers will
    /// substitute with locally-installed or fallback fonts.
    ///
    /// This can be useful for complying with font licenses that prohibit
    /// embedding.  It is incompatible with PDF/A standards (which require
    /// all fonts to be embedded); setting this together with a PDF/A
    /// validator will produce a validation error.
    pub no_embed_fonts: bool,
}
```

Set the default to `false` in the `Default` impl.

Thread the value through `SerializeContext` so that it is accessible during font
serialization. `SerializeContext` already stores a reference to `settings`:

```rust
// serialize.rs -- SerializeContext already has:
pub(crate) settings: SerializeSettings,
```

So `self.settings.no_embed_fonts` is available everywhere `sc` is in scope.

---

## Step 2 -- Skip font program embedding in CID fonts

**File:** `crates/krilla/src/text/cid.rs`, function `CIDFont::serialize()`

The font program is embedded at the end of `serialize()`:

```rust
// Current code (around line 380):
if is_cff {
    font_descriptor.font_file3(data_ref);
} else {
    font_descriptor.font_file2(data_ref);
}
// ... later:
let mut stream = chunk.stream(data_ref, font_stream.encoded_data());
font_stream.write_filters(stream.deref_mut());
```

**Change:** `CIDFont::serialize()` currently does not receive `SerializeContext`
directly -- it receives `sc: &mut SerializeContext`. Use
`sc.settings.no_embed_fonts` to conditionally skip the font program:

```rust
if !sc.settings.no_embed_fonts {
    // Embed font program
    if is_cff {
        font_descriptor.font_file3(data_ref);
    } else {
        font_descriptor.font_file2(data_ref);
    }
    // ... write font stream ...
}
```

When `no_embed_fonts` is `true`:
- Do NOT call `font_descriptor.font_file2/3()`
- Do NOT write the font stream chunk
- Do NOT call `subset_font()` (no need to subset if we are not embedding)
- Still write the font descriptor with name, flags, bbox, ascent/descent etc.
  (viewers use these metrics for substitution)
- Still write widths (for correct glyph positioning)
- Still write the ToUnicode CMap (for copy/paste, search)

The font descriptor without a FontFile reference is a valid PDF construct --
it simply means the font is not embedded and the viewer must find it.

To avoid subsetting when not embedding, restructure the beginning of
`serialize()` to conditionally call `subset_font()`:

```rust
let (font_metrics, global_bbox) = if sc.settings.no_embed_fonts {
    // Skip subsetting; extract metrics from the original font
    let face = self.font.ttf_face();
    let bbox = face.global_bounding_box();
    let rect = Rect::new(
        bbox.x_min as f32, bbox.y_min as f32,
        bbox.x_max as f32, bbox.y_max as f32,
    );
    (self.font.clone(), rect)
} else {
    subset_font(self.font.clone(), &self.glyph_remapper)?
};
```

---

## Step 3 -- Skip font program embedding in Type3 fonts

**File:** `crates/krilla/src/text/type3.rs`, function `Type3Font::serialize()`

Type3 fonts work differently: each glyph is a small content stream (a CharProc)
rather than an embedded font program. Since the glyph appearance is in the
CharProc streams, Type3 fonts must still include those streams even when
`no_embed_fonts` is set -- otherwise glyphs would be invisible.

**Decision:** Type3 fonts are inherently "embedded" (their glyph data IS the
CharProcs). The `no_embed_fonts` flag should only affect CID fonts (which embed
binary OpenType/TrueType data). Type3 fonts are used for color glyphs, SVG
glyphs, and bitmap glyphs -- these are typically not the fonts restricted by
licensing anyway.

No changes needed in `type3.rs`.

---

## Step 4 -- Add validation for PDF/A incompatibility

**File:** `crates/krilla/src/validate.rs` (or wherever validation errors are
collected)

PDF/A and PDF/X standards require all fonts to be embedded. If a user sets both
`no_embed_fonts: true` and a PDF/A validator, this must produce a validation
error.

Add a new `ValidationError` variant:

```rust
pub enum ValidationError {
    // ... existing variants ...

    /// Fonts are not embedded but the selected PDF standard requires it.
    FontsNotEmbedded,
}
```

In the validation pass (called during `SerializeContext::finish()`), check:

```rust
if self.settings.no_embed_fonts && self.settings.configuration.validator().requires_font_embedding() {
    self.validation_errors.push(ValidationError::FontsNotEmbedded);
}
```

If `requires_font_embedding()` does not exist on the validator, add it. All
PDF/A validators (`A1_B` through `A4E`) and PDF/X validators return `true`.
`None` and `UA1` return `false` (UA1 needs tagging but does not mandate
embedding in all cases, though practically fonts should be embedded for
accessibility).

---

## Step 5 -- Handle the Typst consumer side

**File (in Typst repo):** `crates/typst-pdf/src/convert.rs`

Once Krilla supports `no_embed_fonts`, update the Typst integration:

```rust
let settings = SerializeSettings {
    compress_content_streams: true,
    no_device_cs: true,
    ascii_compatible: false,
    xmp_metadata: true,
    cmyk_profile: None,
    configuration: options.standards.config,
    enable_tagging: options.tagged,
    render_svg_glyph_fn: render_svg_glyph,
    no_embed_fonts: !options.embed_fonts,  // NEW
};
```

This replaces the current outline-based workaround in `text.rs` (which converts
text to paths, losing selectability). With native Krilla support, the
`draw_glyphs()` path is used normally and text remains selectable/searchable.

After Krilla is updated, the `draw_glyphs_as_outlines()` function and
`KrillaOutlineBuilder` in `crates/typst-pdf/src/text.rs` can be removed, and
the `handle_text()` function simplified back to always use `draw_glyphs()`.

---

## Step 6 -- Add tests

**Tests to add in Krilla:**

1. **Unit test:** Create a document with `no_embed_fonts: true`, serialize it,
   and verify that the output PDF bytes do NOT contain `/FontFile2` or
   `/FontFile3` entries.

2. **Unit test:** Create a document with `no_embed_fonts: true`, verify that
   `/ToUnicode` CMap is still present and `/Widths` or `/W` entries are still
   written.

3. **Unit test:** Create a document with `no_embed_fonts: true` and a PDF/A
   validator, verify that a `FontsNotEmbedded` validation error is returned.

4. **Integration test:** Render a document with `no_embed_fonts: true` and
   compare the PDF structure (font dictionaries without font programs) against a
   reference.

5. **Snapshot test:** If Krilla uses snapshot/golden-file testing, add a test
   document exercising the flag.

---

## Summary of files to change in Krilla

| File | Change |
|------|--------|
| `crates/krilla/src/serialize.rs` | Add `no_embed_fonts` field to `SerializeSettings`, default `false` |
| `crates/krilla/src/text/cid.rs` | Conditionally skip `subset_font()`, `font_file2/3()`, and font stream writing |
| `crates/krilla/src/validate.rs` | Add `FontsNotEmbedded` validation error for PDF/A incompatibility |
| `crates/krilla/src/lib.rs` | Re-export the new error variant if needed |
| Test files | Add unit and integration tests |

## Summary of follow-up changes in Typst

| File | Change |
|------|--------|
| `crates/typst-pdf/src/convert.rs` | Pass `no_embed_fonts` to `SerializeSettings` |
| `crates/typst-pdf/src/text.rs` | Remove `draw_glyphs_as_outlines` workaround, simplify `handle_text` |
| `crates/typst-pdf/src/convert.rs` | Add error mapping for the new `FontsNotEmbedded` validation error |
| `crates/typst-pdf/Cargo.toml` | Remove `ttf-parser` direct dependency (no longer needed) |
