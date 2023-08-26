
use cyw43::Control;

use embassy_rp::pac::{self, io::vals::Oeover};
use embassy_sync::{blocking_mutex::raw::NoopRawMutex, mutex::Mutex};
use embassy_time::{Duration, Timer};

// This function runs from RAM so it can safely disable flash XIP
// the cortex-m-rt crate doesn't have a great way to mark a ram function yet,
// so we put it in the .data section and mark it inline(never)
#[inline(never)]
#[link_section = ".data"]
fn ram_function() -> bool {
    // Make sure the XIP controller is idle
    loop {
        let xip_status = pac::XIP_CTRL.stat().read();
        if xip_status.fifo_empty() && xip_status.flush_ready() { break; }
    }

    let chip_select = embassy_rp::pac::IO_QSPI.gpio(1);

    // Set chip select to Hi-Z
    chip_select.ctrl().write(|g| g.set_oeover(Oeover::DISABLE));

    // We can't call into the sleep function right now as it's in flash
    cortex_m::asm::delay(2000);

    let button_state = !chip_select.status().read().infrompad();

    // Restore chip select to normal operation so XIP can continue
    chip_select.ctrl().write(|g| g.set_oeover(Oeover::NORMAL));

    button_state
}

#[embassy_executor::task]
pub async fn button_task(control_mutex: &'static Mutex::<NoopRawMutex, Control<'static>>) {
    assert!(pac::SIO.cpuid().read() == 0, "Need to be on core 0");

    let mut prev_state = false;

    loop {
        Timer::after(Duration::from_millis(32)).await;

        // pause the other core so it won't access flash
        embassy_rp::multicore::pause_core1();
        let button_state = critical_section::with(|_| {
            // Wait for all DMA channels accessing flash to finish
            const SRAM_LOWER: u32 = 0x2000_0000;
            for n in 0..12 {
                let ch = embassy_rp::pac::DMA.ch(n);
                while ch.read_addr().read() < SRAM_LOWER && ch.ctrl_trig().read().busy() {}
            }
            // Wait for completion of any streaming reads
            while pac::XIP_CTRL.stream_ctr().read().0 > 0 {}

            ram_function()
        });
        embassy_rp::multicore::resume_core1();

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
