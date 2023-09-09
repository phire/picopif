#![no_std]
#![feature(type_alias_impl_trait)]

mod logger;
mod panic;
mod build_id;
mod persistent_ringbuffer;
mod net;

pub use net::log_drain;
pub use build_id::*;
