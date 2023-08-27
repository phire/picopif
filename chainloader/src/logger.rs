use core::{
    cmp::min,
    future::Future,
    pin::Pin,
    sync::atomic::{AtomicBool, Ordering},
    task::{Context, Poll},
};

use embassy_futures::select::{select, Either};
use embassy_net::{
    tcp::{Error, TcpSocket},
    udp::{PacketMetadata, UdpSocket},
    IpEndpoint, Stack, driver::Driver,
};
use embassy_sync::waitqueue::WakerRegistration;
use embassy_time::{Duration, Timer};
use static_cell::make_static;

static mut CS_RESTORE: critical_section::RestoreState = critical_section::RestoreState::invalid();

// used to detect reentrant calls to global logger
static TAKEN: AtomicBool = AtomicBool::new(false);

static mut LOG_BUFFER: heapless::Deque<u8, 1024> = heapless::Deque::new();
static mut WAKER: WakerRegistration = WakerRegistration::new();
static mut ENCODER: defmt::Encoder = defmt::Encoder::new();

fn push_bytes(bytes: &[u8]) {
    unsafe {
        bytes
            .into_iter()
            .try_for_each(|&b| LOG_BUFFER.push_back(b).ok());
    }
}

#[defmt::global_logger]
struct Logger;

unsafe impl defmt::Logger for Logger {
    fn acquire() {
        let restore = unsafe { critical_section::acquire() };

        if TAKEN.load(Ordering::Relaxed) {
            panic!("defmt logger taken reentrantly");
        }

        TAKEN.store(true, Ordering::Relaxed);
        unsafe { CS_RESTORE = restore };
        unsafe {
            ENCODER.start_frame(push_bytes);
        }
    }
    unsafe fn flush() {
        // Can't actually flush, as that would require async code
        // Though, maybe we could force the executor to prioritize the wifi/net tasks?
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
        critical_section::with(|_| unsafe {
            if LOG_BUFFER.is_empty() {
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
    critical_section::with(|_| unsafe {
        let len = min(buf.len(), LOG_BUFFER.len());
        for i in 0..len {
            // Safety: we are in a critical section, and checked the length above
            buf[i] = LOG_BUFFER.pop_front_unchecked();
        }
        (buf.len(), LOG_BUFFER.len() > 0)
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

async fn broadcast<D: Driver>(stack: &'static Stack<D>, rx_buffer: &mut [u8], tx_buffer: &mut [u8]) {
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
