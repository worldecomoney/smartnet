//! Custom allocator.
//!
//! SmartNet uses the standard library's system allocator.

/// Custom allocator.
pub type Allocator = std::alloc::System;

/// Creates a new [custom allocator][Allocator].
pub const fn new_allocator() -> Allocator {
    std::alloc::System
}
