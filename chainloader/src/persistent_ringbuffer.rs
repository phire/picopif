use core::cmp::min;


const MAGIC: u32 = 0x4c4f4745;

/// A Ring buffer designed to persist across resets
#[repr(C)]
pub struct PersistentRingBuffer {
    magic: u32,
    id: u32,
    full: bool,
    read: usize,
    write: usize,
    capacity: usize,
    persisted: usize,
    storage: [u8; 0],
}

impl PersistentRingBuffer {
    fn valid(&self) -> bool {
        self.magic == MAGIC
            && self.write < self.capacity
            && self.read < self.capacity
            && self.persisted == 0
    }

    /// Safety: total_size must be correct
    pub unsafe fn init(&mut self, id: u32, total_size: usize) {
        let capacity = total_size - core::mem::size_of::<Self>();

        if !(self.valid() && self.capacity == capacity) {
            self.magic = MAGIC;
            self.id = id;
            self.read = 0;
            self.write = 0;
            self.full = false;
            self.capacity = capacity;
            self.persisted = 0;
        } else if self.len() == 0 {
            self.id = id;
            self.persisted = 0;
        } else {
            // Perseve previous ID until any persisted data is reset
            self.persisted = self.len();
        }
    }

    pub fn id(&self) -> u32 {
        self.id
    }

    /// Count of bytes already in the buffer at boot
    pub fn persisted(&self) -> usize {
        self.persisted
    }

    /// Clear the presisted state and update the id
    pub fn reset_presisted(&mut self, id: u32) {
        self.persisted = 0;
        self.id = id;
    }

    fn data(&mut self, start: usize, len: usize) -> &mut [u8] {
        let data =
            unsafe { core::slice::from_raw_parts_mut(self.storage.as_mut_ptr(), self.capacity) };

        data[start..start + len].as_mut()
    }

    pub fn len(&self) -> usize {
        self.capacity - self.free_space()
    }

    pub fn free_space(&self) -> usize {
        if self.full {
            0
        } else if self.read > self.write {
            self.read - self.write
        } else {
            self.capacity - (self.write - self.read)
        }
    }

    pub fn empty(&self) -> bool {
        self.read == self.write && !self.full
    }

    pub fn push_slice(&mut self, slice: &[u8]) -> bool {
        if self.free_space() < slice.len() {
            return false;
        }
        let top = if self.write < self.read{
            self.read
        } else {
            self.capacity
        };
        let len = min(slice.len(), top - self.write);

        let (head, tail) = slice.split_at(len);
        self.data(self.write, len).copy_from_slice(head);
        self.write += head.len();

        if self.write == self.capacity {
            self.data(0, tail.len()).copy_from_slice(tail);
            self.write = tail.len();
        }
        self.full = self.write == self.read && slice.len() > 0;
        return true;
    }

    pub fn pop_to_buf(&mut self, buf: &mut [u8]) -> usize {
        if self.empty() || buf.len() == 0 {
            return 0;
        }
        let top = if self.read < self.write {
            self.write
        } else {
            self.capacity
        };
        let len = min(buf.len(), top - self.read);

        let (head, tail) = buf.split_at_mut(len);
        head.copy_from_slice(self.data(self.read, len));
        self.read += head.len();
        self.full = false;

        if self.read == self.capacity {
            self.read = 0;
            return len + self.pop_to_buf(tail);
        }
        return len;
    }
}
