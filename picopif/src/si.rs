

use core::array::from_fn;

use defmt::println;
use embassy_time::Instant;
use pio_proc::pio_file;

use embassy_rp::{pio::{Pio, Config, ShiftDirection, Direction}, peripherals::*, gpio::{SlewRate, Pull, Input, self, Level}, pio_instr_util, Peripheral, dma::Channel};
use fixed::FixedU32;

use crate::Irqs;


pub async fn sniff2(pio_periph: PIO1, pif_clk: PIN_20, pif_in: PIN_18, pif_out: PIN_19) {
    let mut pif_in = Input::new(pif_clk, Pull::None);

    match pif_in.get_level() {
        Level::Low => println!("pif_in is low"),
        Level::High => println!("pif_in is high"),
    }

    loop {
        pif_in.wait_for_rising_edge().await;
        let mut val = 0u32;
        for _ in 0..32 {
            val = (val << 1) | (pif_in.get_level() as u32);
        }

        println!("rising edge, pattern: {:032b}", val);
    }

}

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

    let program = pio_file!("src/sniffer.pio");

    let loaded_program = pio.common.load_program(&program.program);

    let mut cfg_sniff_in = Config::default();
    cfg_sniff_in.use_program(&loaded_program, &[]);
    cfg_sniff_in.set_in_pins(&[&pif_in, &pif_out, &pif_clk]);
    //cfg_sniff_in.set_in_pins(&[&pif_out, &pif_clk]);
    //cfg_sniff_in.set_jmp_pin(&pif_in);
    cfg_sniff_in.shift_out.direction = ShiftDirection::Left;
    cfg_sniff_in.shift_out.auto_fill = true;
    cfg_sniff_in.shift_in.direction = ShiftDirection::Left;
    cfg_sniff_in.shift_in.auto_fill = true;
    cfg_sniff_in.shift_in.threshold = 30;
    cfg_sniff_in.clock_divider = FixedU32::ONE;

    // let mut cfg_sniff_out = Config::default();
    // cfg_sniff_out.use_program(&loaded_program, &[]);
    // cfg_sniff_out.set_in_pins(&[&pif_clk, &pif_in, &pif_out]);
    // cfg_sniff_out.set_jmp_pin(&pif_out);
    // cfg_sniff_out.shift_out.direction = ShiftDirection::Left;
    // cfg_sniff_out.shift_out.auto_fill = true;
    // cfg_sniff_out.clock_divider = FixedU32::from_bits(0x0200);

    pio.sm0.set_config(&cfg_sniff_in);
    pio.sm0.set_pin_dirs(Direction::In, &[&pif_in, &pif_out, &pif_clk]);

    let mut prev = Instant::now();

    let mut count = 1;
    defmt::println!("Snifffing {} cycles", count);
    const SAMPLES : usize = 1024;
    const WORDS : usize = (SAMPLES * 3).div_ceil(32);

    defmt::println!("{} words containing {} samples", WORDS, SAMPLES);
    let mut dma = dma.into_ref();

    while count > 0 {
        unsafe {
            pio_instr_util::set_y(&mut pio.sm0, SAMPLES as u32);
            pio_instr_util::exec_jmp(&mut pio.sm0, loaded_program.origin)
        }
        let mut data = [0u32; WORDS];

        pio.sm0.set_enable(true);
        //let transfer = pio.sm0.rx().dma_pull(dma.reborrow(), &mut data);
        data[0] = pio.sm0.rx().wait_pull().await;
        let mut idx = 1;
        println!("data[0] = {:032b}", data[0]);

        while idx < WORDS {
            let mut try_read = || {
                for _ in 0..1024 {
                    if let Some(val) = pio.sm0.rx().try_pull() {
                        return Some(val);
                    }
                }
                return None
            };
            if let Some(val) = try_read() {
                data[idx] = val;
                idx += 1;
            } else {
                break;
            }
        }

        let now = Instant::now();
        pio.sm0.set_enable(false);
        let diff = now - prev;
        prev = now;

        let trace = Trace{ len: idx, data };
        defmt::println!("+{:09}: {}", diff.as_micros() as u32, trace);
        count -= 1;
    }
    pio.sm0.set_enable(false);
    defmt::println!("Sniff Done");
}

struct Trace<const N: usize> {
    len: usize,
    data: [u32; N]
}

fn deinterlace3(out: &mut [u32], mut data: u32) {
    for _ in 0..10 {
        out[0] = (out[0] << 1) | ((data >> 27) & 1);
        out[1] = (out[1] << 1) | ((data >> 28) & 1);
        out[2] = (out[2] << 1) | ((data >> 29) & 1);
        data <<=3;
    }
}

impl<const N: usize> defmt::Format for Trace<N> {
    fn format(&self, f: defmt::Formatter) {
        let mut out = [0u32; N];
        //let mut top = 1;
        for i in (0..self.len).step_by(3) {
            for j in i..(i+3) {
                deinterlace3(&mut out[i..], self.data[j]);
            }
        }
        for base in (0..self.len).step_by(6 * 3) {
            let top = (base + 6 * 3).min(self.len);
            defmt::write!(f, "\n{}\n  in: ", base*30);
            for i in (base..top).step_by(3) {
                defmt::write!(f, " {:030b}", out[i]);
            }
            defmt::write!(f, "\n  out:");
            for i in (base..top).step_by(3) {
                defmt::write!(f, " {:030b}", out[i + 1]);
            }
            defmt::write!(f, "\n  clk:");
            for i in (base..top).step_by(3) {
                defmt::write!(f, " {:030b}", out[i + 2]);
            }
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

