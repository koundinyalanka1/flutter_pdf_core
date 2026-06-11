// The plugin migrated from method channels to dart:ffi (see
// lib/src/pdf_core_api.dart). The legacy method-channel layer is retained
// only for backwards compatibility and has no behavior worth testing.
//
// Native-facing behavior is covered by:
//   * rust/  — `cargo test` (parser, ops, text, AI export, crypto, FFI)
//   * example/integration_test — end-to-end FFI smoke tests on device.
void main() {}
