//! API types for the archive service.
//!
//! The archive currently has no HTTP request/response bodies worth
//! typing — it serves repository files directly and subscribes to the
//! runner's `result` Redis channel via loose JSON parsing.
