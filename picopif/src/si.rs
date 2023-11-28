

use core::marker::PhantomData;

use defmt::println;
use embassy_time::{Instant, Duration, Timer};
use pio::{InstructionOperands, InSource};
use pio_proc::pio_file;

use embassy_rp::{pio::{Pio, Config, ShiftDirection, Direction, Instance}, peripherals::*, gpio::{SlewRate, Pull, Input, self, Level, Output, Flex}, pio_instr_util, Peripheral, dma::Channel, pac, interrupt::typelevel::{Handler, Binding}};
use fixed::FixedU32;

use embassy_rp::RegExt;

#[inline(always)]
fn clocks(pio: &mut Pio<PIO1>) -> u32 {
    let x = unsafe { pio_instr_util::get_x(&mut pio.sm1) };
    u32::MAX - x
}

#[derive(Copy, Clone)]
struct LogEntry {
    cmd: u32,
    wait_count: u16,
    diff: i32,
}

static mut SI_LOG: [LogEntry; 20] = [LogEntry { cmd: 0, wait_count: 0, diff: 0 }; 20];
static mut COUNT: usize = 0;

impl defmt::Format for LogEntry {
    fn format(&self, fmt: defmt::Formatter) {
        let cmd = SiCommand::from((self.cmd as u32 >> 9) & 0x3);
        let addr = (self.cmd & 0x1ff) << 2;
        defmt::write!(fmt, "RCP {} {:03x} {:012b} @ +{} ({} cycle wait)", cmd, addr, self.cmd, self.diff, self.wait_count);
    }
}

const INST : u32 = (0x02 << 26 | ((0xbfc0_0140u32) >> 2) & 0x03ff_ffff).reverse_bits();

const INSTS : &[u32] = &[
    0x3C093440u32.reverse_bits(), // 0
    0x40896000u32.reverse_bits(), // 4
    0x3C090006u32.reverse_bits(), // 8
    0x3529E463u32.reverse_bits(), // c
    0x40898000u32.reverse_bits(), // 10
    0x3C08A404u32.reverse_bits(), // 14
    0x00000004u32.reverse_bits(), // 18 <--- causes error???
    0x3C08A404u32.reverse_bits(), // 1c
    0x3C08A404u32.reverse_bits(), // 20
];

struct Si {
    cmd_buf: [u32; 2],
}

static mut SI_INSTANCE : Si = Si {
    cmd_buf: [(32 << 16) | 11, 0u32]
};

pub struct SiInterruptHandler<PIO> {
    _pio: PhantomData<PIO>,
}

impl<PIO: Instance> Handler<PIO::Interrupt> for SiInterruptHandler<PIO> {
    unsafe fn on_interrupt() {
        let pio = PIO::PIO;

        // get the current clock count
        const IN: u16 = InstructionOperands::IN {
            source: InSource::X,
            bit_count: 32,
        }.encode();
        pio.sm(1).instr().write(|instr| instr.set_instr(IN));
        let clk = pio.rxf(1).read();

        let ints = pio.irqs(0).ints().read();
        let si = &mut SI_INSTANCE;
        let count = unsafe { COUNT };

        if !ints.sm0() {
            defmt::warn!("Unexpected interrupt {:x}", ints.0);
            return;
        }
        pio.irq().write(|irq| irq.set_irq(1));

        let mut wait_count = 0u16;

        // Wait for data to be ready
        while (pio.fstat().read().rxempty() & 1) == 1 { wait_count = wait_count.wrapping_add(1); }

        // read data
        let cmd = pio.rxf(0).read();
        let addr = cmd as usize & 0x1ff;

        let inst = if addr < INSTS.len() {
            INSTS[addr]
        } else {
            INST
        };

        //cortex_m::asm::delay(1000);

        pio.txf(0).write_value((32 << 16) | 11 );
        pio.txf(0).write_value( inst );
        unsafe {
            if COUNT < SI_LOG.len() {
                SI_LOG[COUNT] = LogEntry { cmd, wait_count, diff: clk as i32 };
            }
            COUNT += 1;
        }

    }
}

struct fakeIrqs;
unsafe impl<PIO: Instance> Binding<PIO::Interrupt, embassy_rp::pio::InterruptHandler<PIO>> for fakeIrqs {}

pub async fn sniffer<DMA>(dma: impl Peripheral<P = DMA>, pio_periph: PIO1, pif_clk: PIN_20, pif_in: PIN_18, pif_out: PIN_19, nmi: PIN_21, int2: PIN_22) where DMA: Channel {
    let mut pio = Pio::new(pio_periph, fakeIrqs);

    let mut nmi = Flex::new(nmi);
    let mut int2 = Flex::new(int2);
    //let gpio_pif_out = Input::new(unsafe { pif_out.clone_unchecked() }, Pull::None);
    let mut gpio_pif_in = Input::new(unsafe { pif_in.clone_unchecked() }, Pull::Down);

    #[cfg(feature = "net-log")]
    while !net_logger::is_drained() {
        embassy_futures::yield_now().await;
    }

    #[cfg(feature = "rtt-log")] {

    }

    let mut pif_clk: embassy_rp::pio::Pin<PIO1> = pio.common.make_pio_pin(pif_clk);
    pif_clk.set_pull(Pull::None);
    pif_clk.set_schmitt(true);

    let mut pif_in: embassy_rp::pio::Pin<PIO1> = pio.common.make_pio_pin(pif_in);
    pif_in.set_pull(Pull::Down);
    pif_in.set_schmitt(true);

    let mut pif_out: embassy_rp::pio::Pin<PIO1> = pio.common.make_pio_pin(pif_out);
    pif_out.set_pull(Pull::None);
    pif_out.set_slew_rate(SlewRate::Fast);
    pif_out.set_drive_strength(gpio::Drive::_4mA);

    let process = pio_file!(
        "src/si.pio",
        select_program("process")
    );
    let counter = pio_file!(
        "src/si.pio",
        select_program("counter")
    );

    let process = pio.common.load_program(&process.program);
    let loaded_counter = pio.common.load_program(&counter.program);

    let mut cfg_counter = Config::default();
    cfg_counter.use_program(&loaded_counter, &[]);
    cfg_counter.shift_in.auto_fill = true;
    cfg_counter.shift_out.auto_fill = true;
    cfg_counter.clock_divider = FixedU32::ONE;

    pio.sm1.set_config(&cfg_counter);
    unsafe {
        pio_instr_util::set_x(&mut pio.sm1, u32::MAX);
    }
    pio.sm1.set_enable(true);

    let mut cfg_process = Config::default();
    cfg_process.use_program(&process, &[]);
    cfg_process.set_in_pins(&[&pif_in]);
    cfg_process.set_set_pins(&[&pif_out]);
    cfg_process.set_out_pins(&[&pif_out]);
    cfg_process.out_sticky = true;
    cfg_process.shift_in.direction = ShiftDirection::Left;
    //cfg_sniff_in.shift_in.auto_fill = true;
    //cfg_sniff_in.shift_in.threshold = 30;
    cfg_process.shift_out.auto_fill = true;
    cfg_process.shift_out.direction = ShiftDirection::Right;
    cfg_process.clock_divider = FixedU32::ONE;

    unsafe {
        pio_instr_util::set_pindir(&mut pio.sm0, 0);
        pio_instr_util::set_pin(&mut pio.sm0, 0);
    }

    pio.sm0.set_config(&cfg_process);
    pio.sm0.set_pin_dirs(Direction::In, &[&pif_out]);

    pio.sm0.set_enable(true);


    defmt::println!("Ready. INST is {:08x}", INST);

    gpio_pif_in.wait_for_high().await;
    let ready_clks = clocks(&mut pio);
    let rcp_up = Instant::now();

    pif_out.set_pull(Pull::Up);

    unsafe {
        pio_instr_util::set_pindir(&mut pio.sm0, 1);
        pio_instr_util::set_pin(&mut pio.sm0, 1);
    }

    pio.sm0.tx().push(11 | (1 << 31));

    let raw_pio = pac::PIO1;
    raw_pio.irqs(0).inte().write_set(|m| m.set_sm0(true) );

    int2.set_as_output();
    int2.set_drive_strength(gpio::Drive::_4mA);
    int2.set_high();
    nmi.set_as_output();
    nmi.set_drive_strength(gpio::Drive::_4mA);
    nmi.set_high();

    defmt::println!("PIF_IN is now high after {} clocks,", ready_clks);

    // nmi.set_low();
    // cortex_m::asm::delay(1);
    // nmi.set_high();

    let mut prev_clks = ready_clks as i32;
    while unsafe { COUNT } == 0 {
        let clk = clocks(&mut pio) as i32;
        if clk == prev_clks {
            break;
        }
        prev_clks = clk as i32;
        Timer::after(Duration::from_millis(1)).await;
    }
    Timer::after(Duration::from_millis(100)).await;

    // Power down output pin
    pio.sm0.set_pin_dirs(Direction::In, &[&pif_out]);
    pif_out.set_pull(Pull::None);
    int2.set_low();
    int2.set_as_input();
    nmi.set_low();
    nmi.set_as_input();


    println!("Count saw {} requests", unsafe { COUNT });

    pio.sm0.set_enable(false);
    raw_pio.irqs(0).inte().write_set(|m| m.set_sm0(false) );

    prev_clks = ready_clks as i32;

    for entry in unsafe { &SI_LOG[..COUNT.min(SI_LOG.len())] } {
        let diff = match entry.diff.checked_sub(prev_clks) {
        Some(diff) => diff as i32,
            None => -1,
        };
        prev_clks = entry.diff;

        defmt::println!("{:?}", LogEntry { diff, ..*entry });
        Timer::after(Duration::from_millis(1)).await;
    }
}

#[derive(defmt::Format)]
enum SiCommand {
    Write64 = 0,
    Write4 = 1,
    Read64 = 2,
    Read4 = 3,
}

impl From<u32> for SiCommand {
    fn from(cmd: u32) -> Self {
        match cmd {
            0 => SiCommand::Write64,
            1 => SiCommand::Write4,
            2 => SiCommand::Read64,
            3 => SiCommand::Read4,
            _ => panic!("Invalid command {}", cmd),
        }
    }
}

// [0xBFC00000][0x3C093400][LUI t1, 0x3400]        # t1 = 0x34000000
// [0xBFC00004][0x40896000][MTC0 t1, SR]           # SR = t1 (enables CP0, CP1, and FPU registers)
// [0xBFC00008][0x3C090006][LUI t1, 0x0006]        # t1 = 0x00060000
// [0xBFC0000C][0x3529E463][ORI t1, t1, 0xE463]    # t1 = 0x0006E463
// [0xBFC00010][0x40898000][MTC0 t1, Config]       # Config = t1 (sets SysAD port writeback pattern to "D", sets Big-Endian mode, and sets KSEG0 as a cached region)

// [0xBFC00014][0x3C08A404][LUI t0, 0xA404]        # t0 = 0xA4040000
// [0xBFC00018][0x8D080010][LW t0, t0, 0x0010]     # t0 = value stored at 0xA4040010 (RSP_STATUS register)
// [0xBFC0001C][0x31080001][ANDI t0, t0, 0x0001]   # t0 = t0 & 0x0001 (isolates the 'halt' bit)
// [0xBFC00020][0x5100FFFD][BEQL t0, zr, 0xFFFD]   # if t0 == 0, branch to 0xBFC00018 (this is a spin loop, waiting for the RSP to halt)
// [0xBFC00024][0x3C08A404][LUI t0, 0xA404]        # t0 = 0xA4040000

