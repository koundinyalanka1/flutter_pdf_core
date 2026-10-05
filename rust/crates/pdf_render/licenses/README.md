Standalone image codec dependencies
===================================

JPEG 2000 is decoded by hayro-jpeg2000 0.4.1; JBIG2 by hayro-jbig2 0.3.1
and its hayro-ccitt 0.4.0 dependency. Each is available under MIT OR Apache-2.0.
The full license texts are retained here for native binary redistribution.

Source: https://github.com/LaurenzV/hayro

SIMD and image-crate integration are disabled. These are standalone image
codecs, not an alternative PDF engine.

The JPX and JBIG2 crates are vendored with resource limits documented in
`rust/vendor/README.md`. JPEG 2000 ICC profiles and their CC0 license are
retained in `rust/vendor/hayro-jpeg2000/assets/`.
