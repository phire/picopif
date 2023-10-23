#![no_std]
#![no_main]
#![feature(type_alias_impl_trait)]

use core::num::NonZeroU32;

use cyw43::Control;
use cyw43_pio::PioSpi;
use embassy_executor::{Executor, Spawner};
use embassy_net::udp::PacketMetadata;
//use embassy_net::tcp::TcpSocket;
use embassy_net::udp::UdpSocket;
use embassy_net::{Config, Ipv4Address, Stack, StackResources, StaticConfigV4, Ipv4Cidr, IpEndpoint};
use embassy_rp::gpio::{Level, Output};
use embassy_rp::multicore::{spawn_core1, Stack as Core1Stack};
use embassy_rp::peripherals::{CORE1, DMA_CH0, PIN_23, PIN_25, PIO0};
use embassy_rp::pio::{InterruptHandler as PioInterruptHandler, Pio};
use embassy_rp::{bind_interrupts, Peripherals};
use embassy_time::{Duration, Timer};
use static_cell::{make_static, StaticCell};

use embedded_io_async::Write;

static mut CORE1_STACK: Core1Stack<4096> = Core1Stack::new();
static EXECUTOR0: StaticCell<Executor> = StaticCell::new();
static EXECUTOR1: StaticCell<Executor> = StaticCell::new();

pub fn start_core1<F>(core1: CORE1, init: F)
where
    F: FnOnce(Spawner) + Send + 'static,
{
    spawn_core1(core1, unsafe { &mut CORE1_STACK }, move || {
        let executor1 = EXECUTOR1.init(Executor::new());
        executor1.run(|spawner| init(spawner));
    });
}

bind_interrupts!(pub struct Irqs {
    PIO0_IRQ_0 => PioInterruptHandler<PIO0>;
});

#[embassy_executor::task]
async fn wifi_task(
    runner: cyw43::Runner<
        'static,
        Output<'static, PIN_23>,
        PioSpi<'static, PIN_25, PIO0, 0, DMA_CH0>,
    >,
) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn net_task(stack: &'static Stack<cyw43::NetDriver<'static>>) -> ! {
    stack.run().await
}

#[embassy_executor::task]
async fn log_drain_task(stack: &'static Stack<cyw43::NetDriver<'static>>) -> ! {
    net_logger::log_drain(stack).await
}

#[embassy_executor::task]
async fn led_task(mut control: Control<'static>) -> ! {
    let mut led_state = false;
    loop {
        Timer::after(Duration::from_millis(750)).await;
        led_state = !led_state;
        control.gpio_set(0, led_state).await
    }
}

#[embassy_executor::task]
async fn main(spawner: Spawner, p: Peripherals, chainload_state: &'static ChainloadState) {
    let pwr = Output::new(p.PIN_23, Level::Low);
    let cs = Output::new(p.PIN_25, Level::High);
    let mut pio = Pio::new(p.PIO0, Irqs);
    let spi = PioSpi::new(
        &mut pio.common,
        pio.sm0,
        pio.irq0,
        cs,
        p.PIN_24,
        p.PIN_29,
        p.DMA_CH0,
    );

    let mac_address = chainload_state.mac;
    let state = make_static!(cyw43::State::new());

    let (net_device, control, runner) = cyw43::recreate(mac_address, state, pwr, spi);

    spawner.spawn(wifi_task(runner)).unwrap();

    let mut dns_servers = heapless::Vec::new();
    dns_servers.extend(
        chainload_state
            .dns_servers
            .into_iter()
            .flatten()
            .map(|a| a.into()),
    );

    let address = Ipv4Cidr::new(chainload_state.ip.into(), chainload_state.prefix_len);

    let config = Config::ipv4_static(StaticConfigV4 {
        address: address,
        gateway: chainload_state.gateway.map(|a| a.into()),
        dns_servers,
    });

    let stack = &*make_static!(Stack::new(
        net_device,
        config,
        make_static!(StackResources::<2>::new()),
        chainload_state.seed + 1
    ));

    spawner.spawn(net_task(stack)).unwrap();
    spawner.spawn(log_drain_task(stack)).unwrap();
    spawner.spawn(led_task(control)).unwrap();

    let mut rx_buffer = [0; 200];
    let mut tx_buffer = [0; 200];

    // loop {
    //     let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
    //     socket.set_timeout(Some(Duration::from_secs(10)));

    //     if let Err(_) = socket.accept(1235).await {
    //         continue;
    //     }

    //     match socket.write_all(b"Hello world!\r\n").await {
    //         Ok(()) => {}
    //         Err(_) => {
    //             break;
    //         }
    //     }

    //     socket.close();
    //     socket.flush().await.unwrap();
    // }

    let mut rx_meta = [PacketMetadata::EMPTY; 1];
    let mut tx_meta = [PacketMetadata::EMPTY; 1];

    let broadcast_endpoint = IpEndpoint::new(
        embassy_net::IpAddress::Ipv4(address.broadcast().unwrap()),
        4301,
    );

    let mut socket = UdpSocket::new(stack, &mut rx_meta, &mut rx_buffer, &mut tx_meta, &mut tx_buffer);
    socket.bind(4300).unwrap();

    loop {
        Timer::after(Duration::from_millis(32)).await;

        if unsafe { embassy_rp::bootsel::poll_bootsel() } {
            panic!("bootsel pressed");
        };

        // socket.send_to(b"hello", broadcast_endpoint).await.unwrap();

        // let mut buf = [0; 20];
        // let (_count, _ip) = socket.recv_from(&mut buf).await.unwrap();
    }
}

// #[no_mangle]
// pub unsafe extern "C" fn DefaultHandler(_: i16) -> ! {
//     const SCB_ICSR: *const u32 = 0xE000_ED04 as *const u32;
//     let irqn = core::ptr::read_volatile(SCB_ICSR) as u8 as i16 - 16;

//     panic!("DefaultHandler #{:?}", irqn);
// }

#[repr(transparent)]
#[derive(Copy, Clone)]
pub struct Address(NonZeroU32);

impl From<Address> for Ipv4Address {
    #[inline(always)]
    fn from(addr: Address) -> Self {
        let a = addr.0.get();
        let bytes = [a as u8, (a >> 8) as u8, (a >> 16) as u8, (a >> 24) as u8];
        Ipv4Address::from_bytes(&bytes)
    }
}

#[repr(C)]
pub struct ChainloadState {
    seed: u64,
    gateway: Option<Address>,
    dns_servers: [Option<Address>; 3],
    ip: Address,
    prefix_len: u8,
    mac: [u8; 6],
}

#[no_mangle]
pub unsafe extern "C" fn Start(state: &'static ChainloadState) -> u32 {
    let p = Peripherals::steal();


    let executor0 = EXECUTOR0.init(Executor::new());
    executor0.run(|spawner| {
        spawner.spawn(main(spawner, p, state)).unwrap();
    });
}
