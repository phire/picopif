use core::{task::{Poll, Context}, future::Future, ops::DerefMut, cmp::min, pin::Pin};

use embassy_futures::select::{select, Either};
use embassy_net::{tcp::{Error, TcpSocket}, Stack, IpEndpoint, udp::{PacketMetadata, UdpSocket}};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, mutex::{Mutex, MutexGuard}, waitqueue::AtomicWaker};
use embassy_time::{Timer, Duration};
use static_cell::make_static;

static mut LOG_BUFFER: Mutex<CriticalSectionRawMutex, heapless::Deque<u8, 1024>> = Mutex::new(heapless::Deque::new());
static mut GUARD: Option<MutexGuard<'_, CriticalSectionRawMutex, heapless::Deque<u8, 1024>>> = None;
static mut ENCODER: defmt::Encoder = defmt::Encoder::new();

fn push_bytes(bytes: &[u8]) {
    unsafe {
        if let Some(guard) = &mut GUARD {
            let buffer = guard.deref_mut();

            bytes.into_iter().try_for_each(|b| buffer.push_back(*b).ok());
        }
    }
}

#[defmt::global_logger]
struct Logger;

unsafe impl defmt::Logger for Logger {
    fn acquire() {
        // TODO: for correctness, replace mutex with critical_section::acquire
        unsafe {
            if let Ok(mut buffer) =  LOG_BUFFER.try_lock() {
                if buffer.len() > 1000 {
                    // Buffer is full, drop it
                    buffer.clear();
                }
                GUARD = Some(buffer);
                ENCODER.start_frame(push_bytes);
            }
        }
    }
    unsafe fn flush() {
        // Can't actually flush, as that would require async code
    }
    unsafe fn release() {
        ENCODER.end_frame(push_bytes);
        GUARD = None;
        WAKER.wake();
    }
    unsafe fn write(bytes: &[u8]) {
        ENCODER.write(bytes, push_bytes);
    }
}

static WAKER : AtomicWaker = AtomicWaker::new();

struct LogWaitFuture;
impl Future for LogWaitFuture {
    type Output = MutexGuard<'static, CriticalSectionRawMutex, heapless::Deque<u8, 1024>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        critical_section::with(|_| unsafe {
            match LOG_BUFFER.try_lock() {
                Ok(buffer) if !buffer.is_empty() => {
                    return Poll::Ready(buffer);
                }
                _ => {
                    WAKER.register(cx.waker());
                    return Poll::Pending;
                }
            }
        })
    }
}

async fn handle_socket(socket: &mut TcpSocket<'_>) -> Result<(), Error>{
    loop {
        //log::info!("Waiting for log buffer");
        let mut log_buf = LogWaitFuture{}.await;
        //log::info!("have buffer {}", log_buf.len());

        while socket.write_with(|buf| {
                let len = min(buf.len(), log_buf.len());
                let (buf, _) = buf.split_at_mut(len);

                buf.iter_mut().for_each(|dst| { *dst = log_buf.pop_front().unwrap() });
                (buf.len(), log_buf.len() > 0)
            }).await?
        {}
    }
}


async fn broadcast(stack: &'static Stack<cyw43::NetDriver<'static>>, rx_buffer: &mut [u8], tx_buffer: &mut [u8]) {
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

    let mut socket = UdpSocket::new(
        stack,
        &mut rx_meta,
        rx_buffer,
        &mut tx_meta,
        tx_buffer,
    );
    socket.bind(4300).unwrap();
    socket.send_to(b"hello", broadcast_endpoint).await.ok();

    // UdpSocket queues sent packets internally, we need to insert a delay to allow for transmission
    Timer::after(Duration::from_millis(1)).await;
    // Otherwise, closing/dropping the socket will abort the packet
    socket.close();
}

#[embassy_executor::task]
pub async fn log_drain(stack: &'static Stack<cyw43::NetDriver<'static>>) {
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
