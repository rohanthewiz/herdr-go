//! In-process PTY plumbing for the deleted local terminal path.
//! Unreferenced since WS0 stage C (termhost is the only backend);
//! the whole module is deleted in stage D.
#![allow(dead_code, unused_imports)]
pub(crate) mod actor;
pub(crate) mod backend;
#[cfg(unix)]
pub(crate) mod fd;
