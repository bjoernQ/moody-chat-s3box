//! Persists the distilled long-term memory in flash.
//!
//! The memory is a single record `MAGIC | len | crc32 | utf8 text` at the start
//! of the `nvs` data partition. ESP-IDF's NVS library is not used by this
//! firmware, so the partition is free to use as raw storage.

use alloc::string::String;
use alloc::vec::Vec;
use embedded_storage::{ReadStorage, Storage};
use esp_bootloader_esp_idf::partitions::{
    self, DataPartitionSubType, PartitionType, PARTITION_TABLE_MAX_LEN,
};
use esp_storage::FlashStorage;

use crate::println;

const MAGIC: u32 = u32::from_le_bytes(*b"MEM1");
const HEADER_LEN: usize = 12;
/// Upper bound for the stored text; the distillation prompt asks for much less.
pub const MAX_MEMORY_LEN: usize = 3000;

pub struct MemoryStore {
    flash: FlashStorage<'static>,
    offset: u32,
}

impl MemoryStore {
    /// Locates the `nvs` partition. Returns `None` if the partition table has none.
    pub fn new(flash: esp_hal::peripherals::FLASH<'static>) -> Option<Self> {
        // The other core must not run code from flash/PSRAM while flash is written.
        let mut flash = FlashStorage::new(flash).multicore_auto_park();
        let mut table_buffer = [0u8; PARTITION_TABLE_MAX_LEN];
        let table = partitions::read_partition_table(&mut flash, &mut table_buffer).ok()?;
        let nvs = table
            .find_partition(PartitionType::Data(DataPartitionSubType::Nvs))
            .ok()??;
        if (nvs.len() as usize) < HEADER_LEN + MAX_MEMORY_LEN {
            return None;
        }
        let offset = nvs.offset();
        println!("Memory stored at flash offset {:#x}", offset);
        Some(Self { flash, offset })
    }

    /// Returns the stored memory, or an empty string if there is none.
    pub fn load(&mut self) -> String {
        let mut header = [0u8; HEADER_LEN];
        if self.flash.read(self.offset, &mut header).is_err() {
            return String::new();
        }
        let field = |i: usize| u32::from_le_bytes(header[i * 4..i * 4 + 4].try_into().unwrap());
        let (magic, len, crc) = (field(0), field(1) as usize, field(2));
        if magic != MAGIC || len > MAX_MEMORY_LEN {
            return String::new();
        }

        let mut data = alloc::vec![0u8; len];
        if self
            .flash
            .read(self.offset + HEADER_LEN as u32, &mut data)
            .is_err()
            || crc32(&data) != crc
        {
            println!("stored memory is corrupt, starting fresh");
            return String::new();
        }
        String::from_utf8(data).unwrap_or_default()
    }

    pub fn store(&mut self, text: &str) -> Result<(), esp_storage::FlashStorageError> {
        let mut end = text.len().min(MAX_MEMORY_LEN);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let data = &text.as_bytes()[..end];

        let mut record = Vec::with_capacity(HEADER_LEN + data.len());
        record.extend_from_slice(&MAGIC.to_le_bytes());
        record.extend_from_slice(&(data.len() as u32).to_le_bytes());
        record.extend_from_slice(&crc32(data).to_le_bytes());
        record.extend_from_slice(data);
        self.flash.write(self.offset, &record)
    }
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for byte in data {
        crc ^= *byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}
