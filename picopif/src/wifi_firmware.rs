
use embassy_rp::{flash::{Flash, Async}, Peripheral};
use embedded_io_async::{ErrorType, ReadExactError};

//#[link_section = ".flash_only"]
//static FIRMWARE: [u8; include_bytes!("../../embassy/cyw43-firmware/43439A0.bin").len()] = *include_bytes!("../../embassy/cyw43-firmware/43439A0.bin");

//#[link_section = ".flash_only"]
//static CLM: [u8; include_bytes!("../../embassy/cyw43-firmware/43439A0_clm.bin").len()] = *include_bytes!("../../embassy/cyw43-firmware/43439A0_clm.bin");

static FIRMWARE_SIZE: usize = include_bytes!("../../embassy/cyw43-firmware/43439A0.bin").len();
static FIRMWARE_START: usize = (2 * 1024 * 1024) - (250 * 1024); // 0x1c1800

static CLM_SIZE: usize = include_bytes!("../../embassy/cyw43-firmware/43439A0_clm.bin").len();
static CLM_START: usize = (2 * 1024 * 1024) - (10 * 1024); // 0x1fd800

struct FileHandle<'a, FLASH, const SIZE: usize>
where
     FLASH: Peripheral + 'a,
     <FLASH as Peripheral>::P: embassy_rp::flash::Instance,
{
    flash: Flash<'a, FLASH::P, Async, SIZE>,
    start: usize,
    offset: usize,
    size: usize,
}

impl<FLASH, const SIZE: usize> defmt::Format for FileHandle<'_, FLASH, SIZE>
where
     FLASH: Peripheral,
     <FLASH as Peripheral>::P: embassy_rp::flash::Instance,
{
    fn format(&self, f: defmt::Formatter) {
        defmt::write!(f, "FileHandle {{ start: {}, offset: {}, size: {} }}", self.start, self.offset, self.size)
    }
}

#[derive(Debug, defmt::Format)]
struct Error(embassy_rp::flash::Error);

impl embedded_io_async::Error for Error {
    fn kind(&self) -> embedded_io_async::ErrorKind {
        match self.0 {
            embassy_rp::flash::Error::Unaligned | embassy_rp::flash::Error::InvalidCore => embedded_io_async::ErrorKind::Unsupported,
            _ => embedded_io_async::ErrorKind::Other,
        }
    }
}

impl<FLASH, const SIZE: usize> ErrorType for FileHandle<'_, FLASH, SIZE>
where
     FLASH: Peripheral,
     <FLASH as Peripheral>::P: embassy_rp::flash::Instance,
{
    type Error = Error;
}


impl<FLASH, const SIZE: usize> embedded_io_async::Read for FileHandle<'_, FLASH, SIZE>
where
     FLASH: Peripheral,
     <FLASH as Peripheral>::P: embassy_rp::flash::Instance,
{
    async fn read(&mut self, bytes: &mut [u8]) -> Result<usize, Self::Error> {
        defmt::trace!("reading {} bytes from {}", bytes.len(), self.offset);
        if self.offset < self.size {
            let len = bytes.len().min(self.size - self.offset);
            let read_len = len.next_multiple_of(4);
            let addr = self.start + self.offset;
            match self.flash.read(addr as u32, &mut bytes[..read_len]).await {
                Ok(()) => {
                    self.offset += len;
                    Ok(read_len)
                }
                Err(err) => Err(Error(err))
            }
        } else {
            Ok(0)
        }
    }

    async fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), ReadExactError<Self::Error>> {
        match self.read(buf).await {
            Ok(len) if len == buf.len() => Ok(()),
            Ok(_) => Err(ReadExactError::UnexpectedEof),
            Err(err) => Err(ReadExactError::Other(err)),
        }
    }
}

impl<FLASH, const SIZE: usize> embedded_io_async::Seek for FileHandle<'_, FLASH, SIZE>
where
    FLASH: Peripheral,
    <FLASH as Peripheral>::P: embassy_rp::flash::Instance,
{
    async fn seek(&mut self, pos: embedded_io_async::SeekFrom) -> Result<u64, Self::Error> {
        let new_offset = match pos {
            embedded_io_async::SeekFrom::Start(offset) => offset as i64,
            embedded_io_async::SeekFrom::End(offset) => {
                (self.size as i64) - offset
            }
            embedded_io_async::SeekFrom::Current(offset) => {
                self.offset as i64 + offset
            }
        };

        if new_offset < 0 || (new_offset as usize) > self.size {
            return Err(Error(embassy_rp::flash::Error::OutOfBounds));
        }

        self.offset = new_offset as usize;
        Ok(self.offset as u64)
    }
}

fn open<'a, FLASH, DMA>(p_flash: &'a mut FLASH, p_dma: &'a mut DMA, start: usize, size: usize) -> impl embedded_io_async::Read + embedded_io_async::Seek + 'a  + defmt::Format
where
    FLASH: Peripheral,
    <FLASH as Peripheral>::P: embassy_rp::flash::Instance,
    DMA: Peripheral + embassy_rp::dma::Channel,
{
    const FLASH_SIZE: usize = 2 * 1024 * 1024;
    assert!(start < FLASH_SIZE);

    FileHandle::<FLASH, FLASH_SIZE> {
        flash: embassy_rp::flash::Flash::new(p_flash, p_dma),
        start: start,
        offset: 0,
        size
    }
}

pub fn open_firmware<'a, FLASH, DMA>(p_flash: &'a mut FLASH, p_dma: &'a mut DMA) -> impl embedded_io_async::Read + embedded_io_async::Seek + 'a + defmt::Format
where
    FLASH: Peripheral,
    <FLASH as Peripheral>::P: embassy_rp::flash::Instance,
    DMA: Peripheral + embassy_rp::dma::Channel,
{
    open(p_flash, p_dma, FIRMWARE_START, FIRMWARE_SIZE)
}

pub fn open_clm<'a, FLASH, DMA>(p_flash: &'a mut FLASH, p_dma: &'a mut DMA) -> impl embedded_io_async::Read + embedded_io_async::Seek + 'a
where
    FLASH: Peripheral,
    <FLASH as Peripheral>::P: embassy_rp::flash::Instance,
    DMA: Peripheral + embassy_rp::dma::Channel,
{
    open(p_flash, p_dma, CLM_START, CLM_SIZE)
}
