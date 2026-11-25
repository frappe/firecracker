// Copyright 2021 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileTypeExt;

use vm_memory::{GuestMemoryBackend, GuestMemoryError, ReadVolatile, WriteVolatile};

use crate::vstate::memory::{GuestAddress, GuestMemoryMmap};

#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum SyncIoError {
    /// Discard: {0}
    Discard(std::io::Error),
    /// Flush: {0}
    Flush(std::io::Error),
    /// Seek: {0}
    Seek(std::io::Error),
    /// SyncAll: {0}
    SyncAll(std::io::Error),
    /// Transfer: {0}
    Transfer(GuestMemoryError),
}

#[derive(Debug)]
pub struct SyncFileEngine {
    file: File,
}

// SAFETY: `File` is send and ultimately a POD.
unsafe impl Send for SyncFileEngine {}

const BLKDISCARD: libc::Ioctl = 0x1277;
const FALLOC_FL_KEEP_SIZE: libc::c_int = 0x01;
const FALLOC_FL_PUNCH_HOLE: libc::c_int = 0x02;

pub(super) fn file_discard_range(file: &File, range: (u64, u32)) -> Result<u32, std::io::Error> {
    let file_type = file.metadata()?.file_type();
    let (offset, len) = range;

    if len == 0 {
        return Ok(0);
    }
    let discarded = len;
    let len = u64::from(len);

    if file_type.is_block_device() {
        let mut range = [offset, len];
        // SAFETY: file is a valid fd, BLKDISCARD expects a pointer to two u64 values
        // representing byte offset and byte length.
        let ret = unsafe { libc::ioctl(file.as_raw_fd(), BLKDISCARD, range.as_mut_ptr()) };
        if ret < 0 {
            return Err(std::io::Error::last_os_error());
        }
    } else {
        let off = libc::off_t::try_from(offset).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "discard offset overflow")
        })?;
        let len = libc::off_t::try_from(len).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "discard length overflow")
        })?;
        // SAFETY: file is a valid fd and fallocate does not retain the passed values.
        let ret = unsafe {
            libc::fallocate(
                file.as_raw_fd(),
                FALLOC_FL_KEEP_SIZE | FALLOC_FL_PUNCH_HOLE,
                off,
                len,
            )
        };
        if ret < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }

    Ok(discarded)
}

impl SyncFileEngine {
    pub fn from_file(file: File) -> SyncFileEngine {
        SyncFileEngine { file }
    }

    pub fn file(&self) -> &File {
        &self.file
    }

    /// Update the backing file of the engine
    pub fn update_file(&mut self, file: File) {
        self.file = file
    }

    pub fn read(
        &mut self,
        offset: u64,
        mem: &GuestMemoryMmap,
        addr: GuestAddress,
        count: u32,
    ) -> Result<u32, SyncIoError> {
        self.file
            .seek(SeekFrom::Start(offset))
            .map_err(SyncIoError::Seek)?;
        mem.get_slice(addr, count as usize)
            .and_then(|mut slice| Ok(self.file.read_exact_volatile(&mut slice)?))
            .map_err(SyncIoError::Transfer)?;
        Ok(count)
    }

    pub fn write(
        &mut self,
        offset: u64,
        mem: &GuestMemoryMmap,
        addr: GuestAddress,
        count: u32,
    ) -> Result<u32, SyncIoError> {
        self.file
            .seek(SeekFrom::Start(offset))
            .map_err(SyncIoError::Seek)?;
        mem.get_slice(addr, count as usize)
            .and_then(|slice| Ok(self.file.write_all_volatile(&slice)?))
            .map_err(SyncIoError::Transfer)?;
        Ok(count)
    }

    /// Reads or writes the guest buffers `segments` back to back at `offset` with one preadv or
    /// pwritev. Returns the number of bytes it transferred.
    pub fn transfer_vectored(
        &mut self,
        is_write: bool,
        offset: u64,
        mem: &GuestMemoryMmap,
        segments: &[(GuestAddress, u32)],
    ) -> Result<u32, SyncIoError> {
        use crate::vstate::memory::GuestMemoryExtension;

        let iovecs = segments
            .iter()
            .map(|&(addr, len)| {
                let slice = mem.get_slice(addr, len as usize)?;
                let iov_base = slice.ptr_guard_mut().as_ptr().cast();
                let iov_len = slice.len();
                Ok(libc::iovec { iov_base, iov_len })
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(SyncIoError::Transfer)?;
        // SEG_MAX bounds iovcnt. The parser bounds offset to the file size, which fits in off_t.
        let (iovcnt, pos) = (iovecs.len().try_into().unwrap(), offset.try_into().unwrap());
        let transfer = match is_write {
            true => libc::pwritev,
            false => libc::preadv,
        };
        // SAFETY: `get_slice` checked each buffer. Guest memory stays mapped during the call.
        let transferred = unsafe { transfer(self.file.as_raw_fd(), iovecs.as_ptr(), iovcnt, pos) };
        // `Status::from_data` reports a short transfer to the guest as an I/O error.
        let transferred = u32::try_from(transferred).map_err(|_| {
            SyncIoError::Transfer(GuestMemoryError::IOError(std::io::Error::last_os_error()))
        })?;
        if !is_write {
            // preadv does not update the dirty bitmap. Mark the guest buffers dirty for snapshots.
            for &(addr, len) in segments {
                mem.mark_dirty(addr, len as usize);
            }
        }
        Ok(transferred)
    }

    pub fn flush(&mut self) -> Result<(), SyncIoError> {
        // flush() first to force any cached data out of rust buffers.
        self.file.flush().map_err(SyncIoError::Flush)?;
        // Sync data out to physical media on host.
        self.file.sync_all().map_err(SyncIoError::SyncAll)
    }

    pub fn discard(&mut self, range: (u64, u32)) -> Result<u32, SyncIoError> {
        file_discard_range(&self.file, range).map_err(SyncIoError::Discard)
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Seek, SeekFrom, Write};

    use vmm_sys_util::tempfile::TempFile;

    use super::file_discard_range;

    #[test]
    fn test_transfer_vectored() {
        use std::os::unix::fs::FileExt;

        use crate::test_utils::single_region_mem;
        use crate::vstate::memory::{Bytes, GuestAddress};

        let mem = single_region_mem(0x10000);
        let mut engine = super::SyncFileEngine::from_file(TempFile::new().unwrap().into_file());
        engine.file().set_len(0x2000).unwrap();
        let (first, second) = (GuestAddress(0x1000), GuestAddress(0x8000));
        let segments = [(first, 0x1000), (second, 0x200)];
        let data = [[1u8; 0x1000].as_slice(), &[2u8; 0x200]].concat();
        let mut buf = vec![0u8; data.len()];

        // The write puts the guest buffers back to back in the file.
        mem.write_slice(&data[..0x1000], first).unwrap();
        mem.write_slice(&data[0x1000..], second).unwrap();
        let transferred = engine.transfer_vectored(true, 0x400, &mem, &segments);
        assert_eq!(transferred.unwrap(), 0x1200);
        engine.file().read_exact_at(&mut buf, 0x400).unwrap();
        assert_eq!(buf, data);

        // The read puts the data back into the same buffers, in the same order.
        mem.write_slice(&vec![0u8; 0x8200], first).unwrap();
        let transferred = engine.transfer_vectored(false, 0x400, &mem, &segments);
        assert_eq!(transferred.unwrap(), 0x1200);
        mem.read_slice(&mut buf[..0x1000], first).unwrap();
        mem.read_slice(&mut buf[0x1000..], second).unwrap();
        assert_eq!(buf, data);

        // A read past the end of the file returns the short count.
        let transferred = engine.transfer_vectored(false, 0x1f00, &mem, &segments);
        assert_eq!(transferred.unwrap(), 0x100);
    }

    #[test]
    fn test_discard_regular_file() {
        let mut file = TempFile::new().unwrap().into_file();
        file.write_all(&vec![0x5a; 4096]).unwrap();

        match file_discard_range(&file, (1024, 2048)) {
            Ok(discarded) => assert_eq!(discarded, 2048),
            // Some filesystems do not support hole punching; that is a host/filesystem
            // capability limitation, not a test failure for the regular discard path.
            Err(err)
                if matches!(
                    err.raw_os_error(),
                    Some(libc::EOPNOTSUPP) | Some(libc::ENOSYS)
                ) =>
            {
                return;
            }
            Err(err) => panic!("discard failed: {err}"),
        }

        file.seek(SeekFrom::Start(0)).unwrap();
        let mut data = vec![0u8; 4096];
        file.read_exact(&mut data).unwrap();

        assert_eq!(&data[..1024], vec![0x5a; 1024].as_slice());
        assert_eq!(&data[1024..3072], vec![0; 2048].as_slice());
        assert_eq!(&data[3072..], vec![0x5a; 1024].as_slice());
    }
}
