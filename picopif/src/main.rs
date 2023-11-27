#![no_std]
#![no_main]
#![feature(type_alias_impl_trait)]
#![allow(incomplete_features)]
#![feature(generic_const_exprs)]
#![feature(impl_trait_in_fn_trait_return)]

mod button;
mod si;
mod wifi_firmware;

use core::cmp::min;

use defmt::*;
#[cfg(feature = "rtt-log")]
use panic_probe as _;
#[cfg(feature = "rtt-log")]
use defmt_rtt as _;

use cyw43::Control;
use cyw43_pio::PioSpi;
use embassy_executor::Spawner;
use embassy_net::tcp::TcpSocket;
#[cfg(feature = "wifi")]
use embassy_net::{Config, Stack, StackResources};
use embassy_rp::bind_interrupts;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::*;
use embassy_rp::pio::{InterruptHandler as PioInterruptHandler, Pio};
#[cfg(feature = "usb_log")]
use embassy_rp::usb::{Driver, InterruptHandler as UsbInterruptHandler};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::mutex::Mutex;
use embassy_time::{Duration, Instant, Timer};

use embedded_io_async::Write;

use static_cell::make_static;

#[cfg(feature = "wifi")]
const WIFI_NETWORK: &str = env!("WIFI_NETWORK");
#[cfg(feature = "wifi")]
const WIFI_PASSWORD: &str = env!("WIFI_PASSWORD");

#[cfg(feature = "usb_log")]
bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => PioInterruptHandler<PIO0>;
    PIO1_IRQ_0 => PioInterruptHandler<PIO1>;
    USBCTRL_IRQ => UsbInterruptHandler<USB>;
});

#[cfg(not(feature = "usb_log"))]
bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => PioInterruptHandler<PIO0>;
    PIO1_IRQ_0 => PioInterruptHandler<PIO1>;
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

#[cfg(feature = "net-log")]
#[embassy_executor::task]
async fn net_task(stack: &'static Stack<cyw43::NetDriver<'static>>) -> ! {
    stack.run().await
}

#[cfg(feature = "net-log")]
#[embassy_executor::task]
async fn log_drain_task(stack: &'static Stack<cyw43::NetDriver<'static>>) -> ! {
    net_logger::log_drain(stack).await
}

#[cortex_m_rt::pre_init]
unsafe fn pre_init() {
    // Reset spinlock 31, otherwise critical_section_impl might deadlock on reset
    core::arch::asm!("
        ldr r0, =1
        ldr r1, =0xd000017c
        str r0, [r1]
    ");
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let boot = Instant::now();
    let mut p = embassy_rp::init(Default::default());

    println!("Booting picopif ({:08x})", build_id::short_id());

    #[cfg(feature = "run-from-ram")]
    {
        // Enable XIP
        unsafe { embassy_rp::rom_data::flash_enter_cmd_xip() };

        // TODO: Replace XIP background reads with direct QSPI flash reads.
    }

    #[cfg(feature = "usb_log")]
    {
        info!("Starting usb logger");
        let driver = Driver::new(p.USB, Irqs);
        spawner.spawn(logger_task(driver)).unwrap();
        embassy_time::Timer::after(Duration::from_secs(2)).await;
    }

    let wifi_init = Instant::now();
    let control_mutex: &'static Mutex<NoopRawMutex, Control<'static>>;
    #[cfg(feature = "wifi")]
    let net_device;
    {
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
        let firmware_loader = wifi_firmware::open_firmware(&mut p.FLASH, &mut p.DMA_CH1);
        let (_device, control, runner) = cyw43::new(state, pwr, spi, firmware_loader).await;
        spawner.spawn(wifi_task(runner)).unwrap();

        control_mutex = make_static!(Mutex::<NoopRawMutex, _>::new(control));

        #[cfg(feature = "wifi")]
        {
            net_device = _device;
        }
    }

    {
        let clm_loader = wifi_firmware::open_clm(&mut p.FLASH, &mut p.DMA_CH1);
        let mut control = control_mutex.lock().await;
        control.init(clm_loader).await;
        control
            .set_power_management(cyw43::PowerManagementMode::None)
            .await;
    }

    let wifi_init_time = wifi_init.elapsed();

    info!("Booted at {} ms, {}", boot.as_millis(), boot.as_ticks() & 0xffff);
    info!("WiFi init time: {} ms, {}", wifi_init_time.as_millis(), wifi_init_time.as_ticks() & 0xffff);

    spawner
        .spawn(crate::button::button_task(&control_mutex, p.BOOTSEL))
        .unwrap();

    #[cfg(feature = "wifi")]
    {
        let config = Config::dhcpv4(Default::default());
        // Use wifi init as seed.
        // Doesn't need to be cryptographically secure, this seems to give at least a few bits of entropy.
        let seed = wifi_init_time.as_ticks() & 0xffffffff;

        use static_cell::StaticCell;

        static STACK_CELL: StaticCell<Stack<cyw43::NetDriver<'static>>> = StaticCell::new();
        let stack = STACK_CELL.init(Stack::new(
            net_device,
            config,
            make_static!(StackResources::<4>::new()),
            seed,
        ));

        // Start network stack before joining wifi. Otherwise the first DHCP seems to timeout.
        spawner.spawn(net_task(stack)).unwrap();

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
        info!("Connected in {} ms", wifi_join_time.as_millis());

        #[cfg(feature = "net-log")]
        {
            spawner.spawn(log_drain_task(stack)).unwrap();

            info!("Waiting for logs to drain");
            loop {
                Timer::after(Duration::from_millis(100)).await;
                if net_logger::is_drained() {
                    break;
                }
            }

            info!("Logs drained, continuing");
        }
    }

    si::sniffer(p.DMA_CH3, p.PIO1, p.PIN_20, p.PIN_18, p.PIN_19, p.PIN_21, p.PIN_22).await;

    loop {
        Timer::after(Duration::from_millis(1000)).await;
    }

    // let mut rx_buffer = [0; 0x200];
    // let mut tx_buffer = [0; 0x20];

    // loop {
    //     let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
    //     socket.set_timeout(Some(Duration::from_secs(10)));

    //     info!("Listening on port 4303...");
    //     if let Err(e) = socket.accept(4303).await {
    //         warn!("accept error: {:?}", e);
    //         continue;
    //     }
    //     info!("Accepted connection");

    //     handle_ctrl(&mut socket).await.err().map(|e| warn!("ctrl error: {:?}", e));

    //     socket.write_all(b"goodbye").await.ok();
    //     socket.close();
    //     socket.flush().await.ok();
    // }
}

#[derive(defmt::Format)]
pub enum CtrlError {
    ConnectionReset,
    UnknownCommand,
}

impl From<embassy_net::tcp::Error> for CtrlError {
    fn from(_: embassy_net::tcp::Error) -> Self {
        CtrlError::ConnectionReset
    }
}

async fn handle_ctrl(socket: &mut TcpSocket<'_>) -> Result<(), CtrlError> {
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
                    write.write_all(&buf[..bytes]).await.or(Err(CtrlError::ConnectionReset))?;
                    count -= bytes;
                }
            }
            cmd => {
                error!("unknown cmd {}", cmd);
                return  Err(CtrlError::UnknownCommand);
            }
        }

    }
}
