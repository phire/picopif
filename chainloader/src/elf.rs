use defmt::*;

use embassy_net::tcp::TcpReader;
use goblin::elf32;
use plain::Plain;

use core::mem::size_of;
use core::slice;

use embedded_io_async::{Read, ReadExactError};
use embedded_storage_async::nor_flash::NorFlash;
use goblin::elf32::header::{Header, EV_CURRENT};
use goblin::elf32::program_header::{ProgramHeader, PT_LOAD};

pub const FLASH_SIZE: usize = 0x200000;
const PAGE_SIZE: usize = 0x100;
const PAGE_MASK: usize = !(PAGE_SIZE - 1);

pub struct TcpStream<'a> {
    socket: &'a mut TcpReader<'a>,
    pos: i32,
}

impl<'a, 'b> TcpStream<'a> {
    pub fn new(socket: &'a mut TcpReader<'a>) -> Self {
        Self { socket, pos: 0 }
    }

    async fn read_exact(&'b mut self, buf: &mut [u8]) -> Result<(), LoadError> {
        self.socket.read_exact(buf).await?;
        self.pos += buf.len() as i32;
        Ok(())
    }

    async fn read<T>(&'b mut self) -> Result<T, LoadError>
    where
        T: Plain + Sized + Copy,
        [(); core::mem::size_of::<T>()]:,
    {
        let mut buf = [0; core::mem::size_of::<T>()];
        self.read_exact(&mut buf).await?;
        Ok(plain::from_bytes::<T>(&buf)?.clone())
    }

    async fn read_with<F, R>(&'b mut self, f: F) -> Result<R, LoadError>
    where
        F: FnOnce(&[u8]) -> (usize, R),
    {
        self.socket
            .read_with(|buf| {
                let (bytes, result) = f(buf);
                self.pos += bytes as i32;
                (bytes, result)
            })
            .await
            .map_err(|e| e.into())
    }

    async fn seek_absolute(&'b mut self, new_pos: u32) -> Result<(), LoadError> {
        self.seek_relative(new_pos as i32 - self.pos).await
    }

    async fn seek_relative(&'b mut self, count: i32) -> Result<(), LoadError> {
        if count < 0 {
            error!("backwards seek: {}", count);
            Err(LoadError::BackwardsSeek)
        } else {
            info!("seeking {} bytes", count);
            let mut count = count as usize;
            while count > 0 {
                count -= self
                    .read_with(|buf| {
                        let skipping = core::cmp::min(count, buf.len());
                        (skipping, skipping)
                    })
                    .await?;
            }
            Ok(())
        }
    }
}

extern "C" {
    static mut _flash_start: [u8; 0];
    static _flash_end: [u8; 0];
    static mut _ram_start: [u8; 0];
    static _ram_end: [u8; 0];
}

pub async fn load_elf<'a, N: NorFlash>(
    mut s: TcpStream<'a>,
    flash: &mut N,
) -> Result<usize, LoadError> {
    let entry_point;
    let ph_start;
    let ph_entry_size;
    let ph_count;

    enum TempStorage {
        Erased([bool; FLASH_SIZE as usize / embassy_rp::flash::ERASE_SIZE]),
        WriteBuffer([u8; 0x200]),
        //None,
    }
    let mut temp_storage;

    info!("Waiting for elf header...");

    {
        // Phase 1 : Main elf header
        // The main elf header is always at the start of the file and contains a
        // bunch of infomation we need to perserve for later use.
        let header: elf32::header::Header = s.read().await?;

        info!("ELF header {:?}", header.e_ident);

        // Validate the header
        if header.e_ident[..4] != [0x7f, 'E' as u8, 'L' as u8, 'F' as u8]
            || header.e_ident[elf32::header::EI_CLASS] != elf32::header::ELFCLASS32
            || header.e_ident[elf32::header::EI_DATA] != elf32::header::ELFDATA2LSB
            || header.e_ident[elf32::header::EI_VERSION] != EV_CURRENT
            || header.e_type != elf32::header::ET_EXEC
            || header.e_machine != elf32::header::EM_ARM
            || header.e_version != 1
        {
            return Err(LoadError::BadElfHeader);
        }

        entry_point = header.e_entry as usize;
        ph_start = header.e_phoff;
        ph_entry_size = header.e_phentsize as u32;
        ph_count = header.e_phnum as u32;

        if (header.e_phentsize as usize) < size_of::<ProgramHeader>() {
            return Err(LoadError::BadElfHeader);
        }
    }

    info!("ELF header parsed");
    info!("Entry point: 0x{:08x}", entry_point);
    info!("Program headers: {} @ 0x{:08x}", ph_count, ph_start);

    enum Commands<'a> {
        LoadFlash(usize, u32),
        LoadRam(&'a mut [u8]),
        Discard(i32),
    }

    let flash_start: u32 = unsafe { &_flash_start as *const _ } as usize as u32;
    let flash_end: u32 = unsafe { &_flash_end as *const _ } as usize as u32;
    let ram_start: u32 = unsafe { &_ram_start as *const _ } as usize as u32;
    let ram_end: u32 = unsafe { &_ram_end as *const _ } as usize as u32;

    info!("Flash: 0x{:08x} - 0x{:08x}", flash_start, flash_end);
    info!("Ram:   0x{:08x} - 0x{:08x}", ram_start, ram_end);

    #[derive(Debug)]
    struct Command {
        count: u32,
        offset: u32,
    }

    impl Command {
        fn new_load_flash(start: u32, end: u32) -> Self {
            Self {
                count: end - start,
                offset: start,
            }
        }
        fn new_load_ram(start: u32, end: u32) -> Self {
            Self {
                count: end - start,
                offset: start,
            }
        }
        fn new_discard(diff: i32) -> Self {
            Self {
                count: diff as u32,
                offset: 0,
            }
        }

        fn mergeable(&self, other: &Self) -> bool {
            (self.offset + self.count) == other.offset
        }

        fn merge(self, other: Self) -> Self {
            Self {
                count: self.count + other.count,
                offset: self.offset,
            }
        }

        fn page_aligned(&self) -> Self {
            let mut end = (self.offset + self.count) as usize;
            end += (PAGE_SIZE - (end & !PAGE_MASK)) & PAGE_MASK;
            let start = self.offset as usize & PAGE_MASK;
            Self {
                count: (end - start) as u32,
                offset: start as u32,
            }
        }

        fn overlaps(&self, other: &Self) -> bool {
            let Self { count, offset } = self.page_aligned();
            let Self {
                count: other_count,
                offset: other_offset,
            } = other.page_aligned();
            other_offset < (count + offset) && (other_offset + other_count) > offset
        }

        fn as_commands<'a>(&self) -> Commands<'a> {
            let flash_start = unsafe { &_flash_start as *const _ } as usize as u32;
            let flash_end = unsafe { &_flash_end as *const _ } as usize as u32;
            if self.offset == 0 {
                Commands::Discard(self.count as i32)
            } else if self.offset >= flash_start && (self.offset + self.count) <= flash_end {
                Commands::LoadFlash(self.count as usize, self.offset)
            } else {
                let slice = unsafe {
                    slice::from_raw_parts_mut(self.offset as *mut u8, self.count as usize)
                };
                Commands::LoadRam(slice)
            }
        }
    }

    impl defmt::Format for Command {
        fn format(&self, f: defmt::Formatter) {
            match self.as_commands() {
                Commands::LoadFlash(count, offset) => {
                    defmt::write!(
                        f,
                        "LoadFlash {{ count: {:08x}, offset: {:08x} }}",
                        count,
                        offset
                    );
                }
                Commands::LoadRam(slice) => {
                    defmt::write!(
                        f,
                        "LoadRam {{ slice: {:08x} - {:08x} }}",
                        slice.as_ptr() as usize,
                        slice.as_ptr() as usize + slice.len()
                    );
                }
                Commands::Discard(count) => {
                    defmt::write!(f, "Discard {{ count: {:08x} }}", count);
                }
            }
        }
    }

    let mut commands = heapless::Vec::<Command, 25>::new();
    {
        // Phase 2 -- Program Headers
        // The program headers normally follow immediately after the main header.
        // They contain everything needed to load all data into their final memory locations.
        // We take this opportunity to erase any flash blocks that will be written to, and validate
        // the loads are valid.

        let mut load_pos = ph_start + (ph_entry_size * ph_count);

        // Track which flash blocks have been erased
        temp_storage =
            TempStorage::Erased([false; FLASH_SIZE as usize / embassy_rp::flash::ERASE_SIZE]);
        let TempStorage::Erased(erased) = &mut temp_storage else {
            defmt::unreachable!();
        };

        for i in 0..(ph_count as u32) {
            s.seek_absolute(ph_start + ph_entry_size * i).await?;

            let ph: ProgramHeader = s.read().await?;
            if ph.p_type != PT_LOAD || ph.p_filesz == 0 {
                continue;
            }
            match (ph.p_offset as i32) - (load_pos as i32) {
                diff if diff < 0 => return Err(LoadError::LoadsOutOfOrder),
                0 => {}
                diff => {
                    // There is a gap, we need to discard some data
                    commands
                        .push(Command::new_discard(diff))
                        .map_err(|_| LoadError::TooManyLoads)?;
                    load_pos += diff as u32;
                }
            }

            let start = ph.p_vaddr;
            let end = start + ph.p_memsz;
            if start == end {
                continue;
            }

            if start >= flash_start && end < flash_end {
                info!(
                    "Flash load: 0x{:08x} - 0x{:08x} from {:08x}",
                    start, end, load_pos
                );
                let command = {
                    let command = Command::new_load_flash(start, end);
                    match commands.last() {
                        Some(last) if last.mergeable(&command) => {
                            info!("merging flash write: {:?} {:?}", last, command);
                            commands.pop().unwrap().merge(command)
                        }
                        _ => command,
                    }
                };

                // Check for overlaps
                for other in commands.iter() {
                    if other.overlaps(&command) {
                        error!("Overlapping flash write: {:?} {:?}", other, command);
                        return Err(LoadError::OverlappingFlashWrite);
                    }
                }

                // Erase flash blocks
                let from = (start - flash_start) as usize / N::ERASE_SIZE;
                let to = (end - flash_start) as usize / N::ERASE_SIZE;
                for i in from..to {
                    if erased[i] {
                        continue;
                    }

                    let addr = flash_start + (i * N::ERASE_SIZE) as u32;
                    info!(
                        "Erasing flash block at 0x{:08x} to 0x{:08x}",
                        addr,
                        addr + N::ERASE_SIZE as u32
                    );
                    // flash
                    //     .erase(addr, addr + N::ERASE_SIZE as u32)
                    //     .await
                    //     .map_err(|_| LoadError::FlashEraseError)?;
                    erased[i] = true;
                }

                // Queue command
                commands
                    .push(command)
                    .map_err(|_| LoadError::TooManyLoads)?;
            } else if ph.p_vaddr >= ram_start && end < ram_end {
                info!("Ram load: 0x{:08x} - 0x{:08x}", start, end);

                commands
                    .push(Command::new_load_ram(start, end))
                    .map_err(|_| LoadError::TooManyLoads)?;
            } else {
                warn!("Bad load. off: {:x} va: {:x} phys: {:x} filesz: {:x}memsz: {:x} flags: {:x}, align: {:x}", ph.p_offset, ph.p_vaddr, ph.p_paddr, ph.p_filesz, ph.p_memsz, ph.p_flags, ph.p_align);
                return Err(LoadError::BadLoad);
            }

            load_pos += ph.p_filesz;
        }

        if commands.len() == 0 {
            return Err(LoadError::NoLoads);
        }
    }

    // Phase 3 -- Program Text/Data
    // This useally follows immediately after the program headers. We know enough to write it all
    // directly to the correct place as it arrives
    {
        temp_storage = TempStorage::WriteBuffer([0; 0x200]);
        let TempStorage::WriteBuffer(write_buffer) = &mut temp_storage else {
            defmt::unreachable!();
        };

        for cmd in commands.into_iter() {
            match cmd.as_commands() {
                Commands::LoadFlash(mut count, mut offset) => {
                    let mut slice_size = core::cmp::min(count, write_buffer.len());
                    let misalignment = offset as usize & !PAGE_MASK;
                    if slice_size == write_buffer.len() && misalignment != 0 {
                        info!(
                            "Misaligned flash write, old end: 0x{:08x}",
                            offset + slice_size as u32
                        );
                        // make sure our chunks are page aligned
                        slice_size -= PAGE_SIZE - misalignment;
                        info!("New end: 0x{:08x}", offset + slice_size as u32);
                    }

                    while count > 0 {
                        let slice = &mut write_buffer[..slice_size];

                        s.read_exact(slice).await?;
                        info!(
                            "Writing flash at 0x{:08x} - 0x{:08x}",
                            offset,
                            offset + slice.len() as u32
                        );
                        // flash
                        //     .write(offset as u32, slice)
                        //     .await
                        //     .map_err(|_| LoadError::FlashWriteError)?;
                        offset += slice.len() as u32;
                        count -= slice.len();
                        slice_size = core::cmp::min(count, write_buffer.len());
                    }
                }
                Commands::LoadRam(slice) => {
                    let mut next_slice = Some(slice);
                    while next_slice.is_some() {
                        s.read_with(|buf| {
                            let mut dest = next_slice.take().unwrap();
                            if buf.len() < dest.len() {
                                let (head, tail) = dest.split_at_mut(buf.len());
                                (dest, next_slice) = (head, Some(tail));
                            }

                            info!(
                                "Writing ram at 0x{:08x} - 0x{:08x}",
                                dest.as_ptr() as usize,
                                dest.as_ptr() as usize + dest.len()
                            );
                            dest.copy_from_slice(&buf[..dest.len()]);
                            (dest.len(), ())
                        })
                        .await?
                    }
                }
                Commands::Discard(count) => s.seek_relative(count).await?,
            }
        }
    }

    Ok(entry_point)
}

trait FromStream {
    async fn from_stream<S: Read>(stream: &mut S) -> Result<Self, LoadError>
    where
        Self: Sized;
}

#[derive(defmt::Format)]
pub enum LoadError {
    InternalError,
    ConnectionReset,
    UnexpectedEof,
    BadElfMagic,
    BackwardsSeek,
    BadLoad,
    TooManyLoads,
    LoadsOutOfOrder,
    BadElfHeader,
    // FlashEraseError,
    // FlashWriteError,
    OverlappingFlashWrite,
    NoLoads,
    UnknownCommand,
}

impl From<embassy_net::tcp::Error> for LoadError {
    fn from(_: embassy_net::tcp::Error) -> Self {
        LoadError::ConnectionReset
    }
}

impl From<ReadExactError<embassy_net::tcp::Error>> for LoadError {
    fn from(e: ReadExactError<embassy_net::tcp::Error>) -> Self {
        match e {
            ReadExactError::UnexpectedEof => LoadError::UnexpectedEof,
            ReadExactError::Other(e) => e.into(),
        }
    }
}

impl From<plain::Error> for LoadError {
    fn from(_: plain::Error) -> Self {
        LoadError::InternalError
    }
}
