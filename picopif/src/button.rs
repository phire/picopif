
use cyw43::Control;

use embassy_rp::peripherals::BOOTSEL;
use embassy_sync::{blocking_mutex::raw::NoopRawMutex, mutex::Mutex};
use embassy_time::{Duration, Timer};

#[embassy_executor::task]
pub async fn button_task(control_mutex: &'static Mutex::<NoopRawMutex, Control<'static>>, mut bootsel: BOOTSEL) {
    assert!(embassy_rp::pac::SIO.cpuid().read() == 0, "Need to be on core 0");

    let mut prev_state = false;

    loop {
        Timer::after(Duration::from_millis(32)).await;

        let button_state = bootsel.is_pressed();

        if !prev_state && button_state {
            defmt::info!("Resetting");
            // On press, turn LED on
            let mut control = control_mutex.lock().await;
            control.gpio_set(0, true).await;
        }
        else if prev_state && !button_state {
            // on release, turn LED off and reset to USB boot
            let mut control = control_mutex.lock().await;
            control.gpio_set(0, false).await;
            embassy_rp::rom_data::reset_to_usb_boot(0, 0);
        }
        prev_state = button_state;
    }
}
