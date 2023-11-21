

use defmt::{println, error};
use embassy_futures::{yield_now, select::{select, Either}};
use embassy_time::{Instant, Duration, Timer};
use pio_proc::pio_file;

use embassy_rp::{pio::{Pio, Config, ShiftDirection, Direction}, peripherals::*, gpio::{SlewRate, Pull, Input, self, Level, Output}, pio_instr_util, Peripheral, dma::Channel};
use fixed::FixedU32;

use crate::Irqs;

fn clocks(pio: &mut Pio<PIO1>) -> u32 {
    let x = unsafe { pio_instr_util::get_x(&mut pio.sm1) };

    u32::MAX - x
}

pub async fn sniffer<DMA>(dma: impl Peripheral<P = DMA>, pio_periph: PIO1, pif_clk: PIN_20, pif_in: PIN_18, pif_out: PIN_19, nmi: PIN_21, int2: PIN_22) where DMA: Channel {
    let mut pio = Pio::new(pio_periph, Irqs);

    let mut nmi = Output::new(nmi, Level::Low);
    let int2 = Output::new(int2, Level::High);
    let gpio_pif_out = Input::new(unsafe { pif_out.clone_unchecked() }, Pull::None);

    while !net_logger::is_drained() {
        yield_now().await;
    }

    let mut pif_clk: embassy_rp::pio::Pin<PIO1> = pio.common.make_pio_pin(pif_clk);
    pif_clk.set_pull(Pull::None);
    pif_clk.set_schmitt(true);

    let mut pif_in: embassy_rp::pio::Pin<PIO1> = pio.common.make_pio_pin(pif_in);
    pif_in.set_pull(Pull::None);
    pif_in.set_schmitt(true);

    let mut pif_out: embassy_rp::pio::Pin<PIO1> = pio.common.make_pio_pin(pif_out);
    pif_out.set_pull(Pull::Up);

    let read_cmd = pio_file!(
        "src/si.pio",
        select_program("read_cmd")
    );
    let counter = pio_file!(
        "src/si.pio",
        select_program("counter")
    );

    let read_cmd = pio.common.load_program(&read_cmd.program);
    let loaded_counter = pio.common.load_program(&counter.program);

    let mut cfg_counter = Config::default();
    cfg_counter.use_program(&loaded_counter, &[]);
    cfg_counter.shift_in.auto_fill = true;
    cfg_counter.clock_divider = FixedU32::ONE;

    pio.sm1.set_config(&cfg_counter);
    unsafe {
        pio_instr_util::set_x(&mut pio.sm1, u32::MAX);
    }
    pio.sm1.set_enable(true);

    let mut cfg_sniff_in = Config::default();
    cfg_sniff_in.use_program(&read_cmd, &[]);
    cfg_sniff_in.set_in_pins(&[&pif_in]);
    cfg_sniff_in.set_set_pins(&[&pif_out]);
    cfg_sniff_in.set_out_pins(&[&pif_out]);
    cfg_sniff_in.out_sticky = true;
    cfg_sniff_in.shift_in.direction = ShiftDirection::Left;
    //cfg_sniff_in.shift_in.auto_fill = true;
    //cfg_sniff_in.shift_in.threshold = 30;
    cfg_sniff_in.clock_divider = FixedU32::ONE;

    pio.sm0.set_config(&cfg_sniff_in);

    unsafe {
        pio_instr_util::set_pindir(&mut pio.sm0, 1);
        pio_instr_util::set_pin(&mut pio.sm0, 1);
    }
    pio.sm0.set_enable(true);

    defmt::println!("Ready");

    let rcp_up = Instant::now();

    let ready_clks = clocks(&mut pio);

    defmt::println!("PIF_IN is now high after {} clocks, out is {}", ready_clks, gpio_pif_out.is_high() as u8);

    // nmi.set_low();
    // Timer::after(Duration::from_micros(100)).await;
    nmi.set_high();

    let mut prev_clks = ready_clks;

    let mut count = 0;

    loop {
        let timer = Timer::after(Duration::from_millis(1000));
        let cmd_packet = match select(pio.sm0.rx().wait_pull(), timer).await {
            Either::First(cmd_packet) => cmd_packet,
            Either::Second(_) => {
                let clks = clocks(&mut pio);
                match clks.checked_sub(prev_clks) {
                    Some(diff) => defmt::println!("No command for 1s, {}", diff),
                    None => defmt::println!("wrapped {:x} -> {:x}", prev_clks, clks),
                };

                prev_clks = clks;
                continue;
                }
            };
        let cmd_clk = clocks(&mut pio);
        let now = Instant::now();

        let cmd = SiCommand::from((cmd_packet >> 9) & 0x3);
        let addr = (cmd_packet & 0x1ff) << 2;

        let time = now - rcp_up;

        defmt::println!("RCP {} {:x} {:012b} @ {} ({}us)", cmd, addr, cmd_packet, cmd_clk, time.as_micros());

        count += 1;
        if count > 30 {
            break;
        }
    }

    pio.sm0.set_enable(false);
    defmt::println!("NMI is {}", nmi.is_set_high());
    defmt::println!("INT2 is {}", int2.is_set_high());
}

struct Trace {
    offset: usize,
    data: &'static[u32]
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

