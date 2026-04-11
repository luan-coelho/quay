//! Error helpers.
//!
//! Quay uses `anyhow::Result` uniformly at module boundaries — typed error
//! enums were considered but discarded as premature complexity for an app of
//! this size. If a specific call site needs a narrower error type later, add
//! it at that call site instead of defining a top-level enum here.
