# Vendored MIT binding

Source: crates.io `winfsp_wrs` 0.4.0, corresponding upstream
https://github.com/Scille/winfsp_wrs/tree/790484f0d657387696010c054c7e12aa93846be9/winfsp_wrs

The upstream MIT license is preserved in LICENSE. Runtime WinFsp remains a separate
dependency with its own license and FLOSS exception, not relicensed by this copy.

Local change: `SecurityDescriptor::as_bytes` exposes a borrowed immutable slice
of the already owned descriptor for validated read-only Win32 inspection. This
avoids relying on private Rust layout or mutating descriptors to compare ACLs.
No callback or runtime implementation has otherwise been changed.
