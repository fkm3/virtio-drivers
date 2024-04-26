//! Drivers for specific VirtIO devices.

pub mod blk;
#[cfg(feature = "alloc")]
pub mod console;
#[cfg(feature = "alloc")]
pub mod gpu;
#[cfg(feature = "alloc")]
pub mod input;

pub mod net;

pub mod socket;

pub mod sound;

pub(crate) mod common;
