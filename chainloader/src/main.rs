#![no_std]
#![no_main]
#![feature(type_alias_impl_trait)]
#![feature(async_fn_in_trait)]
#![allow(incomplete_features)]
#![feature(generic_const_exprs)]

mod button;
mod elf;

use core::cmp::min;

use defmt::*;
use elf::LoadError;
use embedded_storage_async::nor_flash::NorFlash;

use cyw43::Control;
use cyw43_pio::PioSpi;
use embassy_executor::Spawner;
use embassy_net::tcp::TcpSocket;
use embassy_net::{Config, Stack, StackResources};
use embassy_rp::bind_interrupts;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::*;
use embassy_rp::pio::{InterruptHandler as PioInterruptHandler, Pio};
use embassy_rp::flash::Flash;
#[cfg(feature = "usb_log")]
use embassy_rp::usb::{Driver, InterruptHandler as UsbInterruptHandler};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::mutex::Mutex;
use embassy_time::{Duration, Instant, Timer};

use embedded_io_async::Write;

use static_cell::{make_static, StaticCell};

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
    net_logger::log_drain(stack).await
}

#[embassy_executor::task]
async fn ping_task() -> ! {
    let mut count = 0;
    loop {
        info!("ping {}", count);
        //log::warn!("ping {}", count);
        Timer::after(Duration::from_secs(2)).await;
        count += 1;
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    info!("Booting chainloader ({:08x})", net_logger::short_id());

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
        make_static!(StackResources::<4>::new()),
        seed,
    ));

    spawner.spawn(ping_task()).unwrap();
    spawner.spawn(net_task(stack)).unwrap();
    spawner.spawn(log_drain_task(stack)).unwrap();

    let mut rx_buffer = [0; 0x200];
    let mut tx_buffer = [0; 0x20];

    let mut flash = Flash::<_, _, {elf::FLASH_SIZE}>::new(p.FLASH, p.DMA_CH1);

    loop {
        let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
        socket.set_timeout(Some(Duration::from_secs(10)));

        info!("Listening on port 4303...");
        if let Err(e) = socket.accept(4303).await {
            warn!("accept error: {:?}", e);
            continue;
        }
        info!("Accepted connection");

        handle_ctrl(&mut socket, &mut flash).await.err().map(|e| warn!("ctrl error: {:?}", e));

        socket.write_all(b"goodbye").await.ok();
        socket.close();
        socket.flush().await.ok();
    }
}

async fn handle_ctrl<N: NorFlash>(socket: &mut TcpSocket<'_>, flash: &mut N) -> Result<(), LoadError> {
    //socket.write_all("hello there\n").await?;
    let mut buf = [0u8; 0x20];

    loop {
        let (mut read, mut write) = socket.split();
        read.read(&mut buf[..1]).await.ok();
        info!("cmd {:x}", buf[0]);

        match buf[0] {
            // echo, for testing.
            0xec => {
                read.read(&mut buf[1..2]).await.ok();

                let mut count = buf[1] as usize;
                info!("echoing {} bytes", count);
                while count != 0 {
                    let len = min(count, buf.len());
                    let bytes = read.read(&mut buf[..len]).await?;
                    write.write_all(&buf[..bytes]).await.or(Err(LoadError::ConnectionReset))?;
                    count -= bytes;
                }
            }
            // load elf
            0xef => {
                info!("loading elf");
                let s = elf::TcpStream::new(&mut read);
                let entry = elf::load_elf(s, flash).await?;
                info!("elf entry point: 0x{:08x}", entry);
            }
            cmd => {
                error!("unknown cmd {}", cmd);
                return  Err(LoadError::UnknownCommand);
            }
        }

    }
}
