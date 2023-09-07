use anyhow::ensure;
use ouroboros::self_referencing;
use std::collections::BTreeMap;
use std::{fs, any};
use std::marker::PhantomData;
use std::{collections::HashMap, path::PathBuf, time::SystemTime};

use elf::endian::AnyEndian;
use elf::note::{Note, NoteGnuBuildId};
use elf::section::SectionHeader;
use elf::ElfBytes;
use log::*;

use crate::anyhow;

use defmt_decoder::{ Frame, Location, Table};

pub struct DecoderCache {
    decoders: HashMap<u32, Decoder>,
    paths: HashMap<PathBuf, SystemTime>,
}

pub fn short_id(full_id: &[u8]) -> u32 {
    let mut id = 0;
    let mut next = full_id;
    while next.len() > 4 {
        id ^= u32::from_le_bytes(next[..4].try_into().unwrap());
        next = &next[4..];
    }
    let mut bytes = [0; 4];
    bytes[..next.len()].copy_from_slice(next);
    id ^ u32::from_le_bytes(bytes)
}


impl DecoderCache {
    pub fn new() -> DecoderCache {
        DecoderCache {
            decoders: HashMap::new(),
            paths: HashMap::new(),
        }
    }

    pub fn add_path(&mut self, elf_path: PathBuf) -> anyhow::Result<()> {
        let (id, decoder) = Self::parse_elf(&elf_path)?;

        let modified = fs::metadata(&elf_path)?.modified()?;
        self.paths.insert(elf_path, modified);
        self.decoders.insert(id, decoder);

        Ok(())
    }

    fn parse_elf(elf_path: &PathBuf) -> anyhow::Result<(u32, Decoder)> {
        let bytes = fs::read(elf_path)?;
        let file = ElfBytes::<AnyEndian>::minimal_parse(bytes.as_slice())?;

        //let (section_headers, strtbl) = file.section_headers_with_strtab().unwrap();

        let notes_shdr: SectionHeader = file
            .section_header_by_name(".rodata.gnu_build_id")?
            .ok_or_else(|| anyhow!("no .rodata.gnu_build_id section"))?;

        let notes: Vec<Note<'_>> = file.section_data_as_notes(&notes_shdr)?.collect();
        let id;

        println!("notes: {:?}", notes);
        if let Note::GnuBuildId(gnu_id) = notes[0] {
            println!("gnu_id: {:x?}", gnu_id);
            id = short_id(gnu_id.0);
            println!("short_id: {:x}", id);
        } else {
            anyhow::bail!("no build id found")
        }

        let decoder = decoder_from(bytes.as_slice())?;
        Ok((id, decoder))
    }

    fn rescan_paths(&mut self) -> anyhow::Result<()> {
        for (path, modified) in self.paths.iter_mut() {
            let new_modified = fs::metadata(path)?.modified()?;
            if modified != &new_modified {
                let (new_id, decoder) = Self::parse_elf(path)?;

                *modified = new_modified;
                self.decoders.insert(new_id, decoder);
            }
        }
        Ok(())
    }

    pub fn get(&mut self, id: u32) -> anyhow::Result<&mut Decoder> {
        if !self.decoders.contains_key(&id) {
            self.rescan_paths()?;
            ensure!(self.decoders.contains_key(&id), "Couldn't find decoder for id {:x}", id);
        }
        Ok(self.decoders.get_mut(&id).unwrap())
    }
}

#[self_referencing]
pub struct Decoder {
    table: Table,
    #[borrows(table)]
    #[covariant]
    inner: DecoderInner<'this>,
}

struct DecoderInner<'a> {
    //decoder: Mutex<Box<dyn StreamDecoder + Sync + Send + 'a>>,
    locs: Option<BTreeMap<u64, Location>>,
    //can_recover: bool,
    current_dir: PathBuf,
    phantom_data: PhantomData<&'a ()>,
}

impl DecoderInner<'_> {
    fn location_info(&self, frame: &Frame) -> (Option<String>, Option<u32>, Option<String>) {
        let (mut file, mut line, mut mod_path) = (None, None, None);

        let loc = self.locs.as_ref().and_then(|locs| locs.get(&frame.index()));

        if let Some(loc) = loc {
            // try to get the relative path, else the full one
            let path = loc.file.strip_prefix(&self.current_dir).unwrap_or(&loc.file);

            file = Some(path.display().to_string());
            line = Some(loc.line as u32);
            mod_path = Some(loc.module.clone());
        }

        (file, line, mod_path)
    }

    fn handle_frame(&self, frame: Frame<'_>) {
        let (file, line, mod_path) = self.location_info(&frame);
        defmt_decoder::log::log_defmt(&frame, file.as_deref(), line, mod_path.as_deref());
    }
}

impl Decoder {
    pub fn decode(&mut self, mut bytes: &[u8]) -> anyhow::Result<()> {
        while !bytes.is_empty() {
            // find the zero byte marking the end of the frame
            let Some(zero) = bytes
                .iter()
                .position(|&x| x == 0) else {
                    warn!("Truncated frame");
                    break;
                };

            if zero != 0 {
                let frame = rzcobs_decode(&bytes[..zero])?;
                match self.with_table(|table| table.decode(&frame)) {
                    Ok((frame, _)) => {
                        self.with_inner(|inner| {
                            inner.handle_frame(frame);
                        });
                    }
                    Err(e) => {
                        warn!("Malformed frame: {:?}", e);
                    }
                }
            }

            bytes = &bytes[zero + 1..];
        }
        Ok(())
    }
}

fn rzcobs_decode(data: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut res = vec![];
    let mut data = data.iter().rev().cloned();
    while let Some(x) = data.next() {
        match x {
            0 => return Err(anyhow!("rzcobos malformed")),
            0x01..=0x7f => {
                for i in 0..7 {
                    if x & (1 << (6 - i)) == 0 {
                        res.push(data.next().ok_or(anyhow!("rzcobos malformed"))?);
                    } else {
                        res.push(0);
                    }
                }
            }
            0x80..=0xfe => {
                let n = (x & 0x7f) + 7;
                res.push(0);
                for _ in 0..n {
                    res.push(data.next().ok_or(anyhow!("rzcobos malformed"))?);
                }
            }
            0xff => {
                for _ in 0..134 {
                    res.push(data.next().ok_or(anyhow!("rzcobos malformed"))?);
                }
            }
        }
    }

    res.reverse();
    Ok(res)
}

fn decoder_from(bytes: &[u8]) -> anyhow::Result<Decoder> {
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

            Ok(DecoderInner {
                locs,
                current_dir: std::env::current_dir()?,
                phantom_data: PhantomData,
            })
        },
    }
    .try_build()
    .into()
}
