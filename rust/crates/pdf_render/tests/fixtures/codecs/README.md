Codec regression fixtures
=========================

These fixtures are copied from SerenityOS commit
`3d85104ef2ad231f31ec3de703f24306ea2aa0bf` (BSD 2-Clause; see
SERENITY-LICENSE). Paths relative to that repository:

- `indexed-small.jp2`: `Tests/LibGfx/test-inputs/jpeg2000/indexed-small.jp2`.
  Six RGB palette entries: red, green, blue, cyan, magenta, yellow.
- `rgba-u4.jp2`: `Tests/LibGfx/test-inputs/jpeg2000/openjpeg-lossless-rgba-u4.jp2`.
  Four-bit RGBA OpenJPEG-encoded fixture.
- `bitmap-symbol-global.jbig2`: `Tests/LibGfx/test-inputs/jbig2/bitmap-symbol-global.jbig2`.
  Tests split its global dictionary into the PDF JBIG2Globals stream.
- `bitmap.bmp`: `Tests/LibGfx/test-inputs/bmp/bitmap.bmp`.
  Independent original image, used for exact comparison of every decoded JBIG2 pixel.

Upstream: https://github.com/SerenityOS/serenity/tree/3d85104ef2ad231f31ec3de703f24306ea2aa0bf/Tests/LibGfx/test-inputs

No customer documents are included.
