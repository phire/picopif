
#[repr(C)]
struct ElfNoteSection {
    namesz: u32,
    descsz: u32,
    _type: u32,
    data: [u8; 0],
}

extern "C" {
    static g_note_build_id: ElfNoteSection;
}

pub fn full_id() -> &'static [u8] {
    unsafe {
        let data_ptr = g_note_build_id.data.as_ptr();
        let start = g_note_build_id.namesz as usize;
        let length = g_note_build_id.descsz as usize;

        core::slice::from_raw_parts(data_ptr.add(start), length)
    }
}

pub fn short_id() -> u32 {
    let mut id = 0;
    let mut next = full_id();
    while next.len() > 4 {
        id ^= u32::from_le_bytes(next[..4].try_into().unwrap());
        next = &next[4..];
    }
    let mut bytes = [0; 4];
    bytes[..next.len()].copy_from_slice(next);
    id ^ u32::from_le_bytes(bytes)
}
