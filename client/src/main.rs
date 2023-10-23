use anyhow::{anyhow, Context};
use decoder_cache::DecoderCache;
use log::*;
use std::{
    env, fs,
    io::{Read, Write},
    net::{SocketAddr, TcpStream, UdpSocket},
    path::PathBuf,
    time::Duration, sync::{Mutex, mpsc, OnceLock, MutexGuard},
};

use clap::Parser;

mod decoder_cache;

fn find_endpoint() -> anyhow::Result<(TcpStream, SocketAddr)> {
    let socket = UdpSocket::bind("0.0.0.0:4301")?;
    let mut buf = [0u8; 1024];

    //info!("Waiting for chainloader broadcast");
    loop {
        let (len, address) = socket.recv_from(buf.as_mut())?;
        let s = String::from_utf8_lossy(&buf[..len]).to_string();
        if s == "hello" {
            let timeout = Duration::from_secs(1);
            if let Ok(stream) = TcpStream::connect_timeout(&address, timeout) {
                info!("Found chainloader at {}", address);
                return Ok((stream, address));
            }
        }
    }
}

struct KillableStream {
    stream: TcpStream,
    kill: mpsc::Receiver<()>,
}

impl KillableStream {
    fn new(stream: TcpStream, kill: mpsc::Receiver<()>) -> KillableStream {
        stream.set_read_timeout(Some(Duration::from_millis(1250))).unwrap();

        KillableStream { stream, kill }
    }

    fn read_exact(&mut self, buf: &mut [u8]) -> anyhow::Result<()> {
        loop {
            return match self.stream.read_exact(buf) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if !self.kill .try_recv().is_ok() {
                        continue;
                    }
                    Err(anyhow!("log thread killed"))
                }
                Err(e) => {
                    warn!("read error: {}", e);
                    Err(e.into())
                }
            };
        }
    }
}

fn handle_logstream(mut stream: KillableStream, addr: SocketAddr) -> anyhow::Result<()> {
    let mut buf = [0u8; 4096];
    let mut current_id = 0;
    loop {
        // Header: 12 bytes of MAGIC, ID, BYTES
        let mut header = [0; 12];
        stream.read_exact(header.as_mut_slice())?;

        if &header[0..4] != b"LOGS".as_slice() {
            anyhow::bail!("invalid header {:x?}, {:x?}", header, b"LOGS".as_slice());
        }
        let id = u32::from_le_bytes(header[4..8].try_into().unwrap());
        let bytes = u32::from_le_bytes(header[8..12].try_into().unwrap()) as usize;
        anyhow::ensure!(bytes <= buf.len(), "invalid bytes {}", bytes);

        if current_id != id {
            info!("Starting log stream for {:x} at {}", id, addr);
            current_id = id;
        }

        // Body
        stream.read_exact(&mut buf[..bytes])?;

        // Footer, single u32 of dropped bytes
        let mut footer = [0; 4];
        stream.read_exact(&mut footer)?;
        let dropped = u32::from_le_bytes(footer);

        let valid_bytes = bytes - dropped as usize;

        let mut decoders = lock_decoders();
        let decoder = decoders.get(id)?;

        if let Err(e) = decoder.decode(&buf[..valid_bytes]) {
            warn!("decode error: {}", e);
        }
    }
}

fn handle_control(mut stream: TcpStream, app_path: PathBuf) -> anyhow::Result<()> {
    let mut buf = [0u8; 100];

    info!("testing echo");

    // Test echo
    stream.write_all(b"\xec\x05hello")?;
    stream.read_exact(&mut buf[..5])?;
    info!("Echo: {}", String::from_utf8_lossy(&buf[..5]));

    // Upload elf cmd
    stream.write_all(b"\xef")?;

    info!("Uploading {:?}", &app_path);
    let data = fs::read(app_path)?;
    stream.write_all(data.as_slice())?;

    stream.flush()?;
    info!("Upload complete");
    let len = stream.read(&mut buf)?;
    let s = String::from_utf8_lossy(&buf[..len]).to_string();

    info!("Response: {}", s);
    Ok(())
}

fn should_log(_metadata: &Metadata) -> bool {
    //defmt_decoder::log::is_defmt_frame(metadata)
    true
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about)]
struct Args {
    app: Option<PathBuf>
}

static ARGS : OnceLock<Args> = OnceLock::new();
static DECODERS : OnceLock<Mutex<DecoderCache>> = OnceLock::new();

fn lock_decoders<'a>() -> MutexGuard<'a, DecoderCache> {
    DECODERS.get_or_init(|| Mutex::new(DecoderCache::new())).lock().unwrap()
}

fn add_path(path: &PathBuf) -> anyhow::Result<()> {
    lock_decoders().add_path(path.clone()).with_context(|| format!("Adding {} to decoder cache", path.display()))
}

fn main() -> anyhow::Result<()> {
    defmt_decoder::log::init_logger(None, None, false, should_log);

    ARGS.set(Args::parse()).unwrap();

    let chainloader_path = env::var("CHAINLOADER_PATH").map(|p| p.into()).unwrap_or("target/thumbv6m-none-eabi/release/chainloader".into());

    add_path(&chainloader_path)?;
    if let Some(path) = ARGS.get().unwrap().app.as_ref() {
        add_path(path)?;
    }

    let mut log_join: Option<(mpsc::Sender<()>, std::thread::JoinHandle<()>)> = None;
    loop {
        let (stream, addr) = find_endpoint()?;
        info!("Connected");

        if let Some((kill, join)) = log_join.take() {
            //info!("Killing old log thread");
            kill.send(())?;
            let _ = join.join().unwrap();
        }

        let (tx, rx) = mpsc::channel();
        let join = std::thread::spawn(move || {
            if let Err(e) = handle_logstream(KillableStream::new(stream, rx), addr) {
                warn!("log disconnected: {}", e);
            }
        });

        log_join = Some((tx, join));
    }
}
