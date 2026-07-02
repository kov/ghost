# Test fixtures

- **`FiraCode-Regular.ttf`**, **`FiraCode-Bold.ttf`** —
  [Fira Code](https://github.com/tonsky/FiraCode), © 2014 The Fira Code Project
  Authors, licensed under the **SIL Open Font License 1.1**
  (`FiraCode-LICENSE-OFL.txt`, which covers every weight). Bundled unmodified as
  fixed, ligature-bearing fixtures so the shaping/raster tests are reproducible
  without depending on system-installed fonts. The Bold weight is a genuine
  second face (distinct glyph outlines, shared glyph indices with the Regular) so
  a test can prove the glyph atlas keys per face and never aliases the two. Not
  shipped in any ghost binary.
- **`DejaVuSansMono.ttf`** — [DejaVu Sans Mono](https://dejavu-fonts.github.io/),
  a permissive Bitstream Vera / Arev license (`DejaVu-LICENSE.txt`; the DejaVu
  changes are public domain). Bundled as a *different-coverage* monospace fixture:
  it carries glyphs Fira Code lacks (e.g. ★ U+2605, ❤ U+2764), so a font-fallback
  test can prove an uncovered char is drawn from a fallback face instead of the
  `.notdef` box. Not shipped in any ghost binary.
- **`NotoColorEmoji-COLRv1-subset.ttf`** —
  [Noto Color Emoji](https://github.com/googlefonts/noto-emoji) (COLRv1 build),
  © Google, licensed under the **SIL Open Font License 1.1**
  (`NotoColorEmoji-LICENSE-OFL.txt`), subset to U+1F92A 🤪 and U+2B50 ⭐ with
  `pyftsubset Noto-COLRv1.ttf --unicodes=U+1F92A,U+2B50 --no-hinting
  --name-IDs='*'` (fonttools 4.63). A fixed COLRv1 fixture whose paint graphs
  exercise layers, glyph clips, solid fills, transforms, and linear + radial
  gradients, so the color-raster tests are reproducible without depending on
  system fonts. Not shipped in any ghost binary.
