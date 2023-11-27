#![no_std]
#![feature(type_alias_impl_trait)]

mod logger;
mod panic;
mod persistent_ringbuffer;
mod net;

pub use net::log_drain;

pub fn is_drained() -> bool {
    logger::byte_count() == 0
}