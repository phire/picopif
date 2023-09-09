

use core::{sync::atomic::{AtomicBool, Ordering}, future::Future, task::{Context, Poll}, pin::Pin};

use embassy_sync::waitqueue::WakerRegistration;

use crate::{build_id, persistent_ringbuffer::PersistentRingBuffer};



#[defmt::global_logger]
struct Logger;

unsafe impl defmt::Logger for Logger {
    fn acquire() {
        critical_section::with(|cs| {
            get_ringbuffer(&cs);
        });

        let restore = unsafe { critical_section::acquire() };

        if TAKEN.load(Ordering::Relaxed) {
            // resetting will hopefully allow us print the panic message
            TAKEN.store(false, Ordering::SeqCst);
            core::panic!("defmt logger taken reentrantly");
        }

        TAKEN.store(true, Ordering::Relaxed);
        unsafe {
            CS_RESTORE = restore;
            let cs = critical_section::CriticalSection::new();
            // store the frame start so we can erase everything but the current frame on overflow
            FRAME_START = get_ringbuffer(&cs).get_write_ptr();
            ENCODER.start_frame(push_bytes);
        }
    }

    unsafe fn flush() {
        // Make sure any writes to the persistent ringbuffer have completed
        cortex_m::asm::dsb();
    }

    unsafe fn release() {
        ENCODER.end_frame(push_bytes);
        TAKEN.store(false, Ordering::Relaxed);
        WAKER.wake();

        critical_section::release(CS_RESTORE);
    }

    unsafe fn write(bytes: &[u8]) {
        ENCODER.write(bytes, push_bytes);
    }
}

static mut CS_RESTORE: critical_section::RestoreState = critical_section::RestoreState::invalid();
// used to detect reentrant calls to global logger
static TAKEN: AtomicBool = AtomicBool::new(false);

static mut WAKER: WakerRegistration = WakerRegistration::new();
static mut ENCODER: defmt::Encoder = defmt::Encoder::new();
static mut INITIALIZED: bool = false;
static mut DROPPED: usize = 0;
static mut FRAME_START: usize = 0;

pub fn get_ringbuffer<'cs>(_: &'cs critical_section::CriticalSection) -> &'cs mut PersistentRingBuffer {
    unsafe {
        extern "C" {
            static mut _log_buffer: PersistentRingBuffer;
            static _log_buffer_end: core::ffi::c_void;
        }

        if !INITIALIZED {
            let build_id = build_id::short_id();
            let size = (&_log_buffer_end as *const _ as usize) - (&_log_buffer as *const _ as usize);
            _log_buffer.init(build_id, size);
            INITIALIZED = true;
        }
        &mut _log_buffer
    }
}

pub fn dropped_mut<'cs>(_: &'cs critical_section::CriticalSection) -> &'cs mut usize {
    unsafe { &mut DROPPED }
}

pub fn dropped<'cs>(_: &'cs critical_section::CriticalSection) -> usize {
    unsafe { DROPPED }
}


fn push_bytes(bytes: &[u8]) {
    if TAKEN.load(Ordering::Relaxed) {
        let cs = unsafe { critical_section::CriticalSection::new() };
        let ringbuffer = get_ringbuffer(&cs);
        if !ringbuffer.push_slice(bytes) {
            // The ringbuffer is full, clear everything upto FRAME_START
            unsafe {
                DROPPED += ringbuffer.erase_to(FRAME_START);
            }
            ringbuffer.push_slice(bytes); // try again
        }
    }
}

pub struct LogWaitFuture;
impl Future for LogWaitFuture {
    type Output = usize;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        critical_section::with(|cs| {
            unsafe {
                if DROPPED != 0 {
                    defmt::error!("LogDrain: dropped {} bytes", DROPPED);
                    DROPPED = 0;
                }
            }
            if get_ringbuffer(&cs).empty() {
                unsafe { WAKER.register(cx.waker());}
                return Poll::Pending;
            }
            return Poll::Ready(get_ringbuffer(&cs).len());
        })
    }
}
