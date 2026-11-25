//! Thread-confined plugin VMs, independent of NSH's UI runtime.
mod allocator;
mod collect;
mod globals;
mod host_module;
pub(super) mod loader;
mod natives;
pub(super) mod protocol;
mod runtime;
mod timers;
pub(super) use collect::collect;
pub(super) use runtime::{Session, spawn};

#[cfg(test)]
mod tests;
