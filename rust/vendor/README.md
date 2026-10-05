Vendored image codec resource hardening
=======================================

Sources: crates.io hayro-jbig2 0.3.1 and hayro-jpeg2000 0.4.1 from
https://github.com/LaurenzV/hayro. Upstream source, licenses and JPEG 2000
ICC assets are retained. Only the std configuration is exposed: image-crate,
logging and SIMD integration are excluded from the minimal manifests.

These local changes must be retained or replaced by equivalent upstream
limits when upgrading:

- JBIG2 `bitmap.rs`: check 64 million pixel area before constructing or
  resizing an entropy-decoded bitmap; charge packed bytes before allocation.
- JBIG2 `limits.rs` and `lib.rs`: a per-thread, per-image 128 MiB cumulative
  budget for large decoded buffers, reset on normal returns and unwinding.
  The app always enables std. It counts allocations, not just current live
  bytes, so repeated intermediate buffers cannot evade the limit.
- JBIG2 symbol export uses fallible bitmap clones; remove unused infallible
  Clone derives on bitmap containers. Symbol vectors/context storage,
  pattern shift caches, grayscale/halftone buffers and Huffman trees are
  charged to the same budget.
- JBIG2 bounds flattened symbol references at 65,535, and dynamic Huffman
  tables at 65,536 lines, before potentially expansive allocation.
- JPEG 2000 checks actual tile coding parameters (including tile-part
  overrides) before allocating channel arrays or sorted position traversal:
  262,144 aggregate precincts and 1,048,576 packets.
- JPEG 2000 bounds per-tile precinct/code-block/layer storage before growth:
  262,144 precincts/code blocks and 1,048,576 layer records.

The PDF adapter adds 32 MiB compressed payload, 32 million decoded JPX
component samples, 4,096 tile/segment/reference limits and bounds explicit
JBIG2 regions and a 65,535 aggregate exported/new-symbol limit before
entering either decoder. These are bounded-resource
image limits; they are not a hard process-wide allocator or time sandbox.
Resource-limit errors result in the existing page render warning.

Regression coverage includes an actual tiny JPEG 2000 stream requesting
360,000 precincts, oversized entropy-decoded symbol bitmap construction,
fallible symbol-clone budget accounting and per-image budget reset.
