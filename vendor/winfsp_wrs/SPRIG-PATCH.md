# Vendored MIT binding

Source: crates.io `winfsp_wrs` 0.4.0, corresponding upstream
https://github.com/Scille/winfsp_wrs/tree/790484f0d657387696010c054c7e12aa93846be9/winfsp_wrs

The upstream MIT license is preserved in LICENSE. Runtime WinFsp remains a separate
dependency with its own license and FLOSS exception, not relicensed by this copy.

Local changes:

1. `SecurityDescriptor::as_bytes` exposes a borrowed immutable slice
of the already owned descriptor for validated read-only Win32 inspection. This
avoids relying on private Rust layout or mutating descriptors to compare ACLs.
2. `current_create_uses_default_security` is an original, callback-only accessor
using the existing generated public WinFsp API and request types. It returns
whether the original create request omitted a security descriptor, after checking
context/request presence, request kind, minimum size and named-stream exclusion.
It never exposes pointers or reads the variable request buffer. This lets Sprig
retain its fixed owner without trusting derived descriptor provenance bits.

The upstream runtime and callback trampoline implementations remain unchanged.
