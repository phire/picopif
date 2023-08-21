#![no_std]
#![no_main]
#![feature(type_alias_impl_trait)]
#![feature(async_fn_in_trait)]
#![allow(incomplete_features)]

//mod logger;
mod button;

use core::str::from_utf8;

use cyw43::Control;
use embassy_executor::Spawner;
use embassy_net::tcp::TcpSocket;
use embassy_rp::bind_interrupts;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::*;
use embassy_rp::usb::{Driver, InterruptHandler as UsbInterruptHandler};
use embassy_rp::pio::{Pio, InterruptHandler as PioInterruptHandler};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::mutex::Mutex;
use embassy_time::{Duration, Timer, Instant};
use embassy_net::{Config, Stack, StackResources};
use embedded_io_async::Write;

use static_cell::make_static;

use cyw43_pio::PioSpi;
//use defmt::*;

const WIFI_NETWORK: &str = env!("WIFI_NETWORK");
const WIFI_PASSWORD: &str = env!("WIFI_PASSWORD");

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => PioInterruptHandler<PIO0>;
    USBCTRL_IRQ => UsbInterruptHandler<USB>;
});

//use defmt_rtt as _;

use log::*;

#[cfg(feature = "panic-probe")]
use panic_probe as _;
#[cfg(feature = "panic-reset")]
use panic_reset as _;

#[embassy_executor::task]
async fn wifi_task(
    runner: cyw43::Runner<'static, Output<'static, PIN_23>, PioSpi<'static, PIN_25, PIO0, 0, DMA_CH0>>,
) -> ! {
    runner.run().await
}


#[embassy_executor::task]
async fn logger_task(driver: Driver<'static, USB>) {
    embassy_usb_logger::run!(1024, log::LevelFilter::Info, driver);
}

#[embassy_executor::task]
async fn net_task(stack: &'static Stack<cyw43::NetDriver<'static>>) -> ! {
    stack.run().await
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    let driver = Driver::new(p.USB, Irqs);
    spawner.spawn(logger_task(driver)).unwrap();

    info!("Booting...");

    let wifi_init = Instant::now();
    let control_mutex : &'static Mutex::<NoopRawMutex, Control<'static>>;
    let net_device;
    {
        let fw = include_bytes!("../../embassy/cyw43-firmware/43439A0.bin");

        let pwr = Output::new(p.PIN_23, Level::Low);
        let cs = Output::new(p.PIN_25, Level::High);
        let mut pio = Pio::new(p.PIO0, Irqs);
        let spi = PioSpi::new(&mut pio.common, pio.sm0, pio.irq0, cs, p.PIN_24, p.PIN_29, p.DMA_CH0);

        let state = make_static!(cyw43::State::new());
        let (device, control, runner) =
            cyw43::new(state, pwr, spi, fw).await;
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

    spawner.spawn(crate::button::button_task(&control_mutex)).unwrap();

    let wifi_join = Instant::now();

    loop {
        //control.join_open(WIFI_NETWORK).await;
        info!("Joining...");
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
    let stack = &*make_static!(Stack::new(
        net_device,
        config,
        make_static!(StackResources::<2>::new()),
        seed
    ));

    let wifi_addr = Instant::now();

    spawner.spawn(net_task(stack)).unwrap();

    let config = loop {
        match stack.config_v4() {
            Some(config) => break config.clone(),
            None => {
                Timer::after(Duration::from_millis(50)).await;
            }
        }
    };

    let wifi_addr_time = wifi_addr.elapsed();

    info!("Joined {}: ip={}", WIFI_NETWORK, config.address);
    info!("WiFi join time: {} ms ({} ticks)", wifi_join_time.as_millis(), wifi_join_time.as_ticks());
    info!("WiFi address time: {} ms", wifi_addr_time.as_millis());

    let mut rx_buffer = [0; 4096];
    let mut tx_buffer = [0; 4096];
    let mut buf = [0; 4096];

    loop {
        let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
        socket.set_timeout(Some(Duration::from_secs(10)));

        info!("Listening on {}:1234...", config.address);
        if let Err(e) = socket.accept(1234).await {
            warn!("accept error: {:?}", e);
            continue;
        }

        info!("Received connection from {:?}", socket.remote_endpoint());

        loop {
            let n = match socket.read(&mut buf).await {
                Ok(0) => {
                    warn!("read EOF");
                    break;
                }
                Ok(n) => n,
                Err(e) => {
                    warn!("read error: {:?}", e);
                    break;
                }
            };

            info!("rxd {}", from_utf8(&buf[..n]).unwrap());

            match socket.write_all(&buf[..n]).await {
                Ok(()) => {}
                Err(e) => {
                    warn!("write error: {:?}", e);
                    break;
                }
            };
        }
    }


}
