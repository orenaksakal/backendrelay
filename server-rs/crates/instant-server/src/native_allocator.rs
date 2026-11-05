//! Native executable allocation only; request data, lifetimes and work are unchanged.
//!
//! The same declaration is included by the server's unit tests and selected live
//! integration binaries so their serialization, permission and cancellation
//! contracts exercise the production allocator rather than libc's default.

#[global_allocator]
static NATIVE_ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;
