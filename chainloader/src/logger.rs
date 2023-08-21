


#[defmt::global_logger]
struct Logger;

unsafe impl defmt::Logger for Logger {
    fn acquire() {
        // ...
    }
    unsafe fn flush() {
        // ...
    }
    unsafe fn release() {
        // ...
    }
    unsafe fn write(bytes: &[u8]) {
        // ...
    }
}