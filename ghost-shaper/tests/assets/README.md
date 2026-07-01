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
