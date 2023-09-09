mod imp {
    use core::panic::PanicInfo;
    use core::sync::atomic::{AtomicBool, Ordering};

    #[cfg(feature = "defmt")]
    pub fn print(info: &PanicInfo) {
        defmt::error!("{}", defmt::Display2Format(info));
    }

    #[cfg(not(feature = "defmt"))]
    fn print(_: &PanicInfo) {}

    #[panic_handler]
    fn panic(info: &PanicInfo) -> ! {
        static PANICKED: AtomicBool = AtomicBool::new(false);

        cortex_m::interrupt::disable();

        // Guard against infinite recursion, just in case.
        if !PANICKED.load(Ordering::Relaxed) {
            PANICKED.store(true, Ordering::Relaxed);

            print(info);
        }

        cortex_m::peripheral::SCB::sys_reset();
    }
}
