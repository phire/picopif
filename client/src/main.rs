use anyhow::anyhow;
use log::*;
use ouroboros::self_referencing;
use std::{
    collections::BTreeMap,
    env, fs,
    io::Read,
    net::{SocketAddr, TcpStream, UdpSocket},
    path::PathBuf,
    time::Duration,
};

use defmt_decoder::{
    DecodeError, Frame, Location, StreamDecoder, Table,
};

struct DecoderInner<'a> {
    decoder: Box<dyn StreamDecoder + 'a>,
    locs: Option<BTreeMap<u64, Location>>,
    can_recover: bool,
    current_dir: PathBuf,
}

#[self_referencing]
struct Decoder {
    table: Table,
    #[borrows(mut table)]
    #[covariant]
    inner: DecoderInner<'this>,
}

impl Decoder {
    fn location_info(
        locs: &Option<BTreeMap<u64, Location>>,
        frame: &Frame,
        current_dir: &PathBuf,
    ) -> (Option<String>, Option<u32>, Option<String>) {
        let (mut file, mut line, mut mod_path) = (None, None, None);

        let loc = locs.as_ref().and_then(|locs| locs.get(&frame.index()));

        if let Some(loc) = loc {
            // try to get the relative path, else the full one
            let path = loc.file.strip_prefix(current_dir).unwrap_or(&loc.file);

            file = Some(path.display().to_string());
            line = Some(loc.line as u32);
            mod_path = Some(loc.module.clone());
        }

        (file, line, mod_path)
    }

    fn received(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        self.with_inner_mut(|inner| {
            inner.decoder.received(bytes);
            Self::decode_loop(inner)
        })
    }

    fn decode_loop(inner: &mut DecoderInner) -> anyhow::Result<()> {
        loop {
            match inner.decoder.decode() {
                Ok(frame) => {
                    let (file, line, mod_path) =
                        Self::location_info(&inner.locs, &frame, &inner.current_dir);
                    defmt_decoder::log::log_defmt(
                        &frame,
                        file.as_deref(),
                        line,
                        mod_path.as_deref(),
                    );
                }
                Err(DecodeError::UnexpectedEof) => return Ok(()),
                Err(DecodeError::Malformed) if inner.can_recover => {
                    // if recovery is possible, skip the current frame and continue with new data
                    println!("(HOST) malformed frame skipped");
                    println!("└─ {} @ {}:{}", env!("CARGO_PKG_NAME"), file!(), line!());
                    continue;
                }
                Err(DecodeError::Malformed) => return Err(DecodeError::Malformed.into()), // Otherwise, abort
            }
        }
    }
}

fn new_stream_decoder(elf_path: PathBuf) -> anyhow::Result<Decoder> {
    let bytes = fs::read(elf_path)?;
    let table = Table::parse(&bytes)?.ok_or_else(|| anyhow!(".defmt data not found"))?;
    log::info!("Encoding: {:?}", table.encoding());

    DecoderTryBuilder {
        table,
        inner_builder: |table| {
            let locs = table.get_locations(&bytes)?;
            let locs = if table.indices().all(|idx| locs.contains_key(&(idx as u64))) {
                Some(locs)
            } else {
                warn!("location info is incomplete; it will be omitted");
                None
            };
            let decoder = table.new_stream_decoder();

            Ok(DecoderInner {
                locs,
                can_recover: table.encoding().can_recover(),
                decoder,
                current_dir: std::env::current_dir()?,
            })
        },
    }
    .try_build()
    .into()
}

fn find_chainloader() -> anyhow::Result<SocketAddr> {
    let socket = UdpSocket::bind("0.0.0.0:4301")?;
    let mut buf = [0u8; 1024];

    info!("Waiting for chainloader broadcast");
    loop {
        let (len, address) = socket.recv_from(buf.as_mut())?;
        let s = String::from_utf8_lossy(&buf[..len]).to_string();
        if s == "hello" {
            info!("Found chainloader at {}", address);
            return Ok(address);
        }
    }
}

fn handle_chainloader(mut stream: TcpStream, decoder: &mut Decoder) -> anyhow::Result<()> {
    let mut buf = [0u8; 1500];

    loop {
        let bytes = stream.read(&mut buf)?;

        if bytes == 0 {
            return Ok(());
        }

        decoder.received(&buf[..bytes])?;
    }
}

fn should_log(metadata: &Metadata) -> bool {
    //defmt_decoder::log::is_defmt_frame(metadata)
    true
}

fn main() -> anyhow::Result<()> {
    defmt_decoder::log::init_logger(None, None, false, should_log);

    let mut loader_decoder =
        new_stream_decoder("target/thumbv6m-none-eabi/release/chainloader".into())?;

    loop {
        let addr = find_chainloader()?;
        let timeout = Duration::from_secs(1);

        match std::net::TcpStream::connect_timeout(&addr, timeout) {
            Ok(stream) => {
                info!("Connected");
                if let Err(e) = handle_chainloader(stream, &mut loader_decoder) {
                    warn!("Disconnected with {}", e);
                }
            }
            Err(e) => warn!("Connect timeout: {}", e),
        }
    }
}
