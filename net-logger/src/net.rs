
use embassy_futures::select::{Either, select};
use embassy_net::{
    driver::Driver,
    tcp::TcpSocket,
    udp::{PacketMetadata, UdpSocket},
    IpEndpoint, Stack,
};
use embassy_time::{Duration, Timer};
use embedded_io_async::Write;

use static_cell::make_static;

use crate::{logger::{get_ringbuffer, LogWaitFuture, self}, build_id};

pub async fn log_drain<D: Driver>(stack: &'static Stack<D>) -> ! {
    let tx_buffer = make_static!([0; 0x100]);
    let rx_buffer = make_static!([0; 10]);

    defmt::info!("LogDrain: starting for build {:08x}", build_id::short_id());

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

async fn send_chunk(socket: &mut TcpSocket<'_>, mut bytes: usize, id: u32) -> Result<(), Error> {
    // Header: 12 bytes of MAGIC, ID, BYTES
    let mut header = [0; 12];
    header[0..4].copy_from_slice(b"LOGS");
    header[4..8].copy_from_slice(&id.to_le_bytes());
    header[8..12].copy_from_slice(&bytes.to_le_bytes());
    socket.write_all(&header).await?;

    let mut dropped = 0;

    // Body
    while bytes > 0 {
        socket.write_with(|buf| {
            let len = buf.len().min(bytes);
            critical_section::with(|cs| {
                if logger::dropped(&cs) == 0 {
                    logger::get_ringbuffer(&cs).pop_to_buf(&mut buf[..len]);
                } else {
                    dropped += len;
                    buf[..len].fill(0xff);
                }
                bytes -= len;
                (len, ())
            })
        }).await?;
    }

    if dropped != 0 {
        defmt::error!("{} bytes dropped while sending chunk", dropped);
        critical_section::with(|cs| *logger::dropped_mut(&cs) -= dropped );
    }

    // Footer
    socket.write_all(&dropped.to_le_bytes()).await?;

    Ok(())
}

async fn handle_socket(socket: &mut TcpSocket<'_>) -> Result<(), Error> {
    socket.set_keep_alive(Some(Duration::from_secs(2)));
    let id = build_id::short_id();

    let persisted = critical_section::with(|cs| {
        let rb = get_ringbuffer(&cs);
        let persisted = rb.persisted();
        let mut dropped = logger::dropped(&cs);
        if dropped == 0 {
            if persisted != 0 {
                defmt::println!("LogDrain: {} bytes from {:08x} persisted across reset", persisted, rb.id());
                Some((persisted, rb.id()))
            } else {
                None
            }
        } else {
            if persisted != 0 {
                defmt::error!("LogDrain: dropped {} bytes from {:08x} ", persisted, rb.id());
                dropped -= persisted;
                rb.reset_presisted(id);
            }
            defmt::error!("LogDrain: dropped {} bytes before connecting", dropped);
            *logger::dropped_mut(&cs) = 0;
            None
        }
    });

    if let Some((bytes, prev_id)) = persisted {
        send_chunk(socket, bytes, prev_id).await?;
        critical_section::with(|cs| get_ringbuffer(&cs).reset_presisted(id));
    }

    loop {
        // This read allows us to detect a connection reset
        let read = socket.read_with(|buf| (buf.len(), ()));

        // Block until there are bytes in the log buffer (or connection resets)
        match select(LogWaitFuture {}, read).await {
            Either::First(bytes) => {
                send_chunk(socket, bytes, id).await?;
            }
            Either::Second(Err(embassy_net::tcp::Error::ConnectionReset)) => {
                // The read failed, the connection has reset
                defmt::println!("LogDrain: connection reset");
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

#[derive(defmt::Format)]
enum Error {
    ConnectionReset,
}

impl From<embassy_net::tcp::Error> for Error {
    fn from(_: embassy_net::tcp::Error) -> Self {
        Error::ConnectionReset
    }
}

