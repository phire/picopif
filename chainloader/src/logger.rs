use core::{
    cmp::min,
    future::Future,
    pin::Pin,
    sync::atomic::{AtomicBool, Ordering},
    task::{Context, Poll}, ffi::c_void,
};

use embassy_futures::select::{select, Either};
use embassy_net::{
    driver::Driver,
    tcp::{Error, TcpSocket},
    udp::{PacketMetadata, UdpSocket},
    IpEndpoint, Stack,
};
use embassy_sync::waitqueue::WakerRegistration;
use embassy_time::{Duration, Timer};
use static_cell::make_static;

use crate::{build_id, persistent_ringbuffer::PersistentRingBuffer};

static mut CS_RESTORE: critical_section::RestoreState = critical_section::RestoreState::invalid();

// used to detect reentrant calls to global logger
static TAKEN: AtomicBool = AtomicBool::new(false);

static mut WAKER: WakerRegistration = WakerRegistration::new();
static mut ENCODER: defmt::Encoder = defmt::Encoder::new();
static mut INITIALIZED: bool = false;

fn get_ringbuffer<'cs>(_: &'cs critical_section::CriticalSection) -> &'cs mut PersistentRingBuffer {
    unsafe {
        extern "C" {
            static mut _log_buffer: PersistentRingBuffer;
            static _log_buffer_end: c_void;
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

fn push_bytes(bytes: &[u8]) {
    if TAKEN.load(Ordering::Relaxed) {
        let cs = unsafe { critical_section::CriticalSection::new() };
        get_ringbuffer(&cs).push_slice(bytes);
    }
}

#[defmt::global_logger]
struct Logger;

unsafe impl defmt::Logger for Logger {
    fn acquire() {
        critical_section::with(|cs| {
            get_ringbuffer(&cs);
        });

        let restore = unsafe { critical_section::acquire() };

        if TAKEN.load(Ordering::Relaxed) {
            // resetting will hopefully allow us to recover
            TAKEN.store(false, Ordering::SeqCst);
            panic!("defmt logger taken reentrantly");
        }

        TAKEN.store(true, Ordering::Relaxed);
        unsafe {
            CS_RESTORE = restore;
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

struct LogWaitFuture;
impl Future for LogWaitFuture {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        critical_section::with(|cs| unsafe {
            if get_ringbuffer(&cs).empty() {
                WAKER.register(cx.waker());
                return Poll::Pending;
            }
            return Poll::Ready(());
        })
    }
}

fn copy_to_buf(buf: &mut [u8]) -> (usize, bool) {
    // The socket is ready to send upto buf.len() bytes
    // Take a critical section while we move bytes out of LOG_BUFFER
    critical_section::with(|cs| {
        let rb = get_ringbuffer(&cs);
        let bytes = rb.pop_to_buf(buf);
        (bytes, !rb.empty())
    })
}

async fn handle_socket(socket: &mut TcpSocket<'_>) -> Result<(), Error> {
    socket.set_keep_alive(Some(Duration::from_secs(2)));

    loop {
        // This read allows us to detect a connection reset
        let read = socket.read_with(|buf| (buf.len(), ()));

        // Block until there are bytes in the log buffer (or connection resets)
        match select(LogWaitFuture {}, read).await {
            Either::First(_) => {
                // Send data in log buffer
                while socket.write_with(copy_to_buf).await? {}
            }
            Either::Second(Err(Error::ConnectionReset)) => {
                // The read failed, the connection has reset
                return Ok(());
            }
            Either::Second(Ok(_)) => {
                defmt::error!("LogDrain: unexpected message");
            }
        };
    }
}

async fn broadcast<D: Driver>(
    stack: &'static Stack<D>,
    rx_buffer: &mut [u8],
    tx_buffer: &mut [u8],
) {
    let address = loop {
        match stack.config_v4() {
            Some(config) => break config.address,
            None => {
                Timer::after(Duration::from_millis(50)).await;
            }
        }
    };

    let mut rx_meta = [PacketMetadata::EMPTY; 1];
    let mut tx_meta = [PacketMetadata::EMPTY; 1];

    let broadcast_endpoint = IpEndpoint::new(
        embassy_net::IpAddress::Ipv4(address.broadcast().unwrap()),
        4301,
    );

    let mut socket = UdpSocket::new(stack, &mut rx_meta, rx_buffer, &mut tx_meta, tx_buffer);
    socket.bind(4300).unwrap();
    socket.send_to(b"hello", broadcast_endpoint).await.ok();

    // UdpSocket queues sent packets internally, we need to insert a delay to allow for transmission
    Timer::after(Duration::from_millis(1)).await;
    // Otherwise, closing/dropping the socket will abort the packet
    socket.close();
}

pub async fn log_drain<D: Driver>(stack: &'static Stack<D>) -> ! {
    let tx_buffer = make_static!([0; 0x100]);
    let rx_buffer = make_static!([0; 10]);

    defmt::info!("LogDrain: starting for build {:08x}", build_id::short_id());

    let (persisted, id) = critical_section::with(|cs| {
        let rs = get_ringbuffer(&cs);
        (rs.persisted(), rs.id())
    });

    if persisted > 0 {
        defmt::info!("LogDrain: {} bytes from {:08x} persisted across reset", persisted, id);
    }

    loop {
        broadcast(stack, rx_buffer, tx_buffer).await;

        let mut socket = TcpSocket::new(stack, rx_buffer, tx_buffer);

        let accept = socket.accept(4300);
        let timeout = Timer::after(Duration::from_secs(1));

        match select(accept, timeout).await {
            Either::First(Ok(())) => {
                defmt::info!("LogDrain: Accepted connection");
                if let Err(e) = handle_socket(&mut socket).await {
                    defmt::error!("LogDrain: Socket error: {:?}", e);
                }
            }
            Either::First(Err(e)) => {
                defmt::error!("LogDrain: accept error: {:?}", e);
                Timer::after(Duration::from_secs(1)).await;
            }
            Either::Second(_) => {
                socket.abort();
            }
        }
    }
}
