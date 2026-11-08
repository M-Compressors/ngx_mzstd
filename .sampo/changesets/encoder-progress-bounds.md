---
cargo/ngx_mzstd: patch
---

The body filter checks that each encoder call reports no more input consumed and output produced than the buffers it was given before it moves the buffer pointers, and otherwise logs an error and aborts the response. mzstd never reports more, so responses are unchanged; the check keeps the module's pointer arithmetic from relying on that.
