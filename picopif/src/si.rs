

use core::{array::from_fn, borrow::BorrowMut, ops::Neg};

use defmt::{println, error};
use embassy_futures::yield_now;
use embassy_time::Instant;
use pio_proc::pio_file;

use embassy_rp::{pio::{Pio, Config, ShiftDirection, Direction}, peripherals::*, gpio::{SlewRate, Pull, Input, self, Level}, pio_instr_util, Peripheral, dma::Channel};
use fixed::FixedU32;

use crate::Irqs;

const WORDS : usize = 10000;
static mut DATA: [u32; WORDS] = [0u32; WORDS];

pub async fn sniffer<DMA>(dma: impl Peripheral<P = DMA>, pio_periph: PIO1, pif_clk: PIN_20, pif_in: PIN_18, pif_out: PIN_19) where DMA: Channel {
    let mut pio = Pio::new(pio_periph, Irqs);

    let mut pif_clk: embassy_rp::pio::Pin<PIO1> = pio.common.make_pio_pin(pif_clk);
    pif_clk.set_pull(Pull::None);
    pif_clk.set_schmitt(true);
    pif_clk.set_input_sync_bypass(true);
    pif_clk.set_slew_rate(SlewRate::Fast);

    let mut pif_in: embassy_rp::pio::Pin<PIO1> = pio.common.make_pio_pin(pif_in);
    pif_in.set_pull(Pull::None);
    pif_in.set_schmitt(true);
    pif_in.set_input_sync_bypass(true);
    pif_in.set_slew_rate(SlewRate::Fast);

    let mut pif_out: embassy_rp::pio::Pin<PIO1> = pio.common.make_pio_pin(pif_out);
    pif_out.set_pull(Pull::None);
    pif_out.set_schmitt(true);
    pif_out.set_input_sync_bypass(true);
    pif_out.set_slew_rate(SlewRate::Fast);

    let program = pio_file!("src/sniff_cmd.pio");
    let counter = pio_file!("src/counter.pio");

    let loaded_program = pio.common.load_program(&program.program);
    let loaded_counter = pio.common.load_program(&counter.program);

    let mut cfg_sniff_in = Config::default();
    cfg_sniff_in.use_program(&loaded_program, &[]);
    cfg_sniff_in.set_in_pins(&[&pif_in, &pif_out, &pif_clk]);
    //cfg_sniff_in.set_in_pins(&[&pif_out, &pif_clk]);
    //cfg_sniff_in.set_jmp_pin(&pif_in);
    cfg_sniff_in.shift_in.direction = ShiftDirection::Left;
    //cfg_sniff_in.shift_in.auto_fill = true;
    //cfg_sniff_in.shift_in.threshold = 30;
    cfg_sniff_in.clock_divider = FixedU32::ONE;

    // let mut cfg_sniff_out = Config::default();
    // cfg_sniff_out.use_program(&loaded_program, &[]);
    // cfg_sniff_out.set_in_pins(&[&pif_clk, &pif_in, &pif_out]);
    // cfg_sniff_out.set_jmp_pin(&pif_out);
    // cfg_sniff_out.shift_out.direction = ShiftDirection::Left;
    // cfg_sniff_out.shift_out.auto_fill = true;
    // cfg_sniff_out.clock_divider = FixedU32::from_bits(0x0200);

    pio.sm0.set_config(&cfg_sniff_in);
    pio.sm0.set_enable(true);

    let mut cfg_counter = Config::default();
    cfg_counter.use_program(&loaded_counter, &[]);
    cfg_counter.shift_in.auto_fill = true;
    cfg_counter.clock_divider = FixedU32::ONE;
    pio.sm1.set_config(&cfg_counter);

    unsafe {
        pio_instr_util::set_x(&mut pio.sm1, u32::MAX);
    }
    pio.sm1.set_enable(true);

    let mut prev = Instant::now();

    let mut count = 0;
    //defmt::println!("Snifffing {} cycles", count);

    //defmt::println!("{} words containing {} samples", WORDS, SAMPLES);
    let mut dma = dma.into_ref();

    let prev = 0;
    let mut stop = u32::MAX;

    let data_slice = unsafe { &mut DATA[0..] };
    while count < data_slice.len() {
        let cmd = match pio.sm0.rx().try_pull() {
            Some(cmd) => cmd,
            None => {
                stop -= 1;
                if stop < 10 {
                    break;
                }
                continue
            }
        };

        stop = 1000;

        let cycles = (u32::MAX - 5000000) - unsafe { pio_instr_util::get_x(&mut pio.sm1) };

        data_slice[count] = cmd << 21 | (cycles & 0x1f_ffff);
        count += 1;
    }

    for i in 0..count {
        let cmd = data_slice[i];
        let cmd_type = SiCommand::from(cmd >> 30);
        let addr =  0xbfc0_0000 | ((cmd >> 21 & 0x1ff) << 2);
        let cycles = cmd & 0x1f_ffff;
        defmt::println!("{:?} {:08x} @ {}", cmd_type, addr, cycles);
        if i & 0x3f == 0x3f {
            while !net_logger::is_drained() {
                yield_now().await;
            }
        }
    }
    pio.sm0.set_enable(false);
    defmt::println!("Sniff Done");
}

struct Trace {
    offset: usize,
    data: &'static[u32]
}

fn deinterlace3(out: &mut [u32]) {
    let mut data = out[0];
    let mut more_data = [out[1], out[2]];
    out[0] = 0;
    out[1] = 0;
    out[2] = 0;
    for _ in 0..3 {
        for _ in 0..10 {
            out[0] = (out[0] << 1) | ((data >> 27) & 1);
            out[1] = (out[1] << 1) | ((data >> 28) & 1);
            out[2] = (out[2] << 1) | ((data >> 29) & 1);
            data <<=3;
        }
        data = more_data[0];
        more_data[0] = more_data[1];
    }
}

impl defmt::Format for Trace {
    fn format(&self, f: defmt::Formatter) {
        for base in (0..self.data.len()).step_by(6 * 3) {
            let top = (base + 6 * 3).min(self.data.len());
            defmt::write!(f, "\n{}\n  in: ", (base + self.offset) * 30);
            for i in (base..top).step_by(3) {
                defmt::write!(f, " {:030b}", self.data[i]);
            }
            defmt::write!(f, "\n  out:");
            for i in (base..top).step_by(3) {
                defmt::write!(f, " {:030b}", self.data[i + 1]);
            }
            defmt::write!(f, "\n  clk:");
            for i in (base..top).step_by(3) {
                defmt::write!(f, " {:030b}", self.data[i + 2]);
            }
        }
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

