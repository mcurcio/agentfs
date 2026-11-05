//! Byte-call frames for the tree actor (git-model §3).

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use super::{Error, Record, Result};

const MAGIC: &[u8; 4] = b"AFP1";
const MAX_FRAME: usize = 96 << 20;

pub(super) struct Wal(File);

impl Wal {
    pub fn open(path: &Path) -> Result<(Self, Vec<Record>)> {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let mut offset = 0;
        let mut records = Vec::new();
        while offset < bytes.len() {
            let rest = &bytes[offset..];
            if rest.len() < 16 {
                break; // Only an incomplete frame is a disposable torn tail.
            }
            if &rest[..4] != MAGIC {
                return Err(Error::CorruptJournal(offset));
            }
            let len = u32::from_le_bytes(rest[4..8].try_into().expect("four bytes")) as usize;
            let crc = u32::from_le_bytes(rest[8..12].try_into().expect("four bytes"));
            let header_crc = u32::from_le_bytes(rest[12..16].try_into().expect("four bytes"));
            // Protect the length too: a flipped length must not discard an acknowledged frame.
            if crc32fast::hash(&rest[..12]) != header_crc {
                return Err(Error::CorruptJournal(offset));
            }
            if len > MAX_FRAME {
                return Err(Error::CorruptJournal(offset));
            }
            if rest.len() < 16 + len {
                break;
            }
            let payload = &rest[16..16 + len];
            if crc32fast::hash(payload) != crc {
                return Err(Error::CorruptJournal(offset));
            }
            let record: Record = serde_json::from_slice(payload)?;
            if record.seq != records.len() as u64 + 1 {
                return Err(Error::CorruptJournal(offset));
            }
            records.push(record);
            offset += 16 + len;
        }
        file.set_len(offset as u64)?;
        file.sync_all()?;
        file.seek(SeekFrom::End(0))?;
        File::open(path.parent().expect("journal has parent"))?.sync_all()?;
        Ok((Self(file), records))
    }

    pub fn append(&mut self, record: &Record) -> Result<()> {
        let payload = serde_json::to_vec(record)?;
        if payload.len() > MAX_FRAME {
            return Err(Error::TooLarge);
        }
        let before = self.0.stream_position()?;
        let mut header = Vec::from(MAGIC.as_slice());
        header.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        header.extend_from_slice(&crc32fast::hash(&payload).to_le_bytes());
        let header_crc = crc32fast::hash(&header);
        header.extend_from_slice(&header_crc.to_le_bytes());
        let result = (|| {
            self.0.write_all(&header)?;
            self.0.write_all(&payload)?;
            self.0.sync_all()
        })();
        if let Err(e) = result {
            // The actor stops on ANY append failure, even if rollback fails.
            let _ = self.0.set_len(before);
            let _ = self.0.sync_all();
            return Err(e.into());
        }
        Ok(())
    }

    pub fn sync(&self) -> Result<()> {
        Ok(self.0.sync_all()?)
    }
}
