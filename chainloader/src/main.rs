#![no_std]
#![no_main]
#![feature(type_alias_impl_trait)]
#![feature(async_fn_in_trait)]
#![allow(incomplete_features)]
#![feature(generic_const_exprs)]

mod button;
mod logger;
mod build_id;
mod persistent_ringbuffer;


use defmt::*;
use logger::*;

use cyw43::Control;
use embassy_executor::Spawner;
use embassy_net::tcp::TcpSocket;
use embassy_net::{Config, Stack, StackResources};
use embassy_rp::bind_interrupts;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::*;
use embassy_rp::pio::{InterruptHandler as PioInterruptHandler, Pio};
#[cfg(feature = "usb_log")]
use embassy_rp::usb::{Driver, InterruptHandler as UsbInterruptHandler};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::mutex::Mutex;
use embassy_time::{Duration, Instant};

use static_cell::{make_static, StaticCell};

use cyw43_pio::PioSpi;

const WIFI_NETWORK: &str = env!("WIFI_NETWORK");
const WIFI_PASSWORD: &str = env!("WIFI_PASSWORD");

#[cfg(feature = "usb_log")]
bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => PioInterruptHandler<PIO0>;
    USBCTRL_IRQ => UsbInterruptHandler<USB>;
});

#[cfg(not(feature = "usb_log"))]
bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => PioInterruptHandler<PIO0>;
});

#[cfg(feature = "panic-probe")]
use panic_probe as _;
#[cfg(feature = "panic-reset")]
use panic_reset as _;

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

#[cfg(feature = "usb_log")]
#[embassy_executor::task]
async fn logger_task(driver: Driver<'static, USB>) {
    embassy_usb_logger::run!(1024, log::LevelFilter::Info, driver);
}

#[embassy_executor::task]
async fn net_task(stack: &'static Stack<cyw43::NetDriver<'static>>) -> ! {
    stack.run().await
}

#[embassy_executor::task]
async fn log_drain_task(stack: &'static Stack<cyw43::NetDriver<'static>>) -> ! {
    log_drain(stack).await
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    info!("Booting chainloader ({:08x})", build_id::short_id());

    #[cfg(feature = "usb_log")]
    {
        info!("Starting usb logger");
        let driver = Driver::new(p.USB, Irqs);
        spawner.spawn(logger_task(driver)).unwrap();
        embassy_time::Timer::after(Duration::from_secs(2)).await;
    }

    let wifi_init = Instant::now();
    let control_mutex: &'static Mutex<NoopRawMutex, Control<'static>>;
    let net_device;
    {
        let fw = include_bytes!("../../embassy/cyw43-firmware/43439A0.bin");

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

        let state = make_static!(cyw43::State::new());
        let (device, control, runner) = cyw43::new(state, pwr, spi, fw).await;
        spawner.spawn(wifi_task(runner)).unwrap();

        control_mutex = make_static!(Mutex::<NoopRawMutex, _>::new(control));
        net_device = device;
    }

    {
        let clm = include_bytes!("../../embassy/cyw43-firmware/43439A0_clm.bin");
        let mut control = control_mutex.lock().await;
        control.init(clm).await;
        control
            .set_power_management(cyw43::PowerManagementMode::None)
            .await;
    }

    let wifi_init_time = wifi_init.elapsed();

    info!("WiFi init time: {:?}", wifi_init_time);

    spawner
        .spawn(crate::button::button_task(&control_mutex))
        .unwrap();

    let wifi_join = Instant::now();

    loop {
        info!("Joining {}", WIFI_NETWORK);
        let mut control = control_mutex.lock().await;
        match control.join_wpa2(WIFI_NETWORK, WIFI_PASSWORD).await {
            Ok(_) => break,
            Err(err) => {
                info!("join failed with status={}", err.status);
            }
        }
    }

    let wifi_join_time = wifi_join.elapsed();

    let config = Config::dhcpv4(Default::default());

    // Use wifi init and connection times as a seed.
    // Doesn't need to be cryptographically secure, this seems to give at least 16 bits of entropy.
    let seed = (wifi_init_time.as_ticks() & 0xffffffff) * (wifi_join_time.as_ticks() & 0xffffffff);

    static STACK_CELL: StaticCell<Stack<cyw43::NetDriver<'static>>> = StaticCell::new();
    let stack = STACK_CELL.init(Stack::new(
        net_device,
        config,
        make_static!(StackResources::<3>::new()),
        seed,
    ));

    spawner.spawn(net_task(stack)).unwrap();
    spawner.spawn(log_drain_task(stack)).unwrap();

    let mut rx_buffer = [0; 0x200];
    let mut tx_buffer = [0; 0x20];

    let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
    socket.set_timeout(Some(Duration::from_secs(10)));

    loop {
        defmt::info!("Listening on port 4303...");
        if let Err(e) = socket.accept(4303).await {
            warn!("accept error: {:?}", e);
            continue;
        }
        defmt::info!("Accepted connection");

        socket.write(b"goodbye").await.ok();
        socket.close();
    }
}
