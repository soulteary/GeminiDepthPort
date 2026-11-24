//! Unix private content-addressed store. A directory FD pins the store inode;
//! openat/linkat never follow an attacker-supplied filename or replace a record.
use crate::{
    ContractError, ErrorClass, Result, SnapshotId, check,
    snapshot::{SnapshotBinding, SnapshotEnvelope},
};
use ring::rand::{SecureRandom, SystemRandom};
use std::{
    ffi::CString,
    fs::{DirBuilder, File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    },
    path::Path,
};
pub struct SnapshotStore {
    directory: File,
}
fn io(_: std::io::Error) -> ContractError {
    ContractError::new(ErrorClass::Io, "SNAPSHOT_STORE_IO")
}
impl SnapshotStore {
    pub fn open(path: &Path) -> Result<Self> {
        check(
            path.is_absolute(),
            ErrorClass::InvalidInput,
            "STORE_ABSOLUTE_PATH_REQUIRED",
        )?;
        match DirBuilder::new().mode(0o700).create(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(io(e)),
        }
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
            .map_err(io)?;
        let m = directory.metadata().map_err(io)?;
        check(
            m.is_dir() && m.mode() & 0o777 == 0o700 && m.uid() == unsafe { libc::geteuid() },
            ErrorClass::Integrity,
            "STORE_NOT_PRIVATE",
        )?;
        Ok(Self { directory })
    }
    fn open_file(&self, name: &str, flags: i32) -> Result<File> {
        let name = CString::new(name)
            .map_err(|_| ContractError::new(ErrorClass::InvalidInput, "STORE_FILENAME"))?;
        let fd = unsafe {
            libc::openat(
                self.directory.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0 {
            return Err(io(std::io::Error::last_os_error()));
        }
        Ok(unsafe { File::from_raw_fd(fd) })
    }
    pub fn read(&self, id: &SnapshotId, expected: &SnapshotBinding) -> Result<SnapshotEnvelope> {
        let file = self.open_file(&format!("{id}.json"), libc::O_RDONLY | libc::O_NONBLOCK)?;
        let m = file.metadata().map_err(io)?;
        check(
            m.is_file()
                && m.mode() & 0o777 == 0o600
                && m.uid() == unsafe { libc::geteuid() }
                && m.len() <= 4 * 1024 * 1024
                && m.nlink() == 1,
            ErrorClass::Integrity,
            "SNAPSHOT_FILE_NOT_PRIVATE",
        )?;
        let mut bytes = Vec::new();
        file.take(4 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(io)?;
        let out = SnapshotEnvelope::parse(&bytes, expected)?;
        check(
            &out.snapshot_id == id,
            ErrorClass::Integrity,
            "SNAPSHOT_FILENAME_MISMATCH",
        )?;
        Ok(out)
    }
    pub fn publish(&self, envelope: &SnapshotEnvelope) -> Result<SnapshotId> {
        let bytes = envelope.to_bytes()?;
        let mut random = [0_u8; 16];
        SystemRandom::new()
            .fill(&mut random)
            .map_err(|_| ContractError::new(ErrorClass::Io, "STORE_RANDOM_FAILED"))?;
        let name = format!(".pending-{}", crate::lower_hex(&random));
        let mut file = self.open_file(&name, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL)?;
        // Cleanup also occurs on a write/fsync/link error. A crash may leave an
        // unaddressed private .pending file, never a partially addressed snapshot.
        let operation = (|| {
            file.write_all(&bytes).map_err(io)?;
            file.sync_all().map_err(io)?;
            let source = CString::new(name.as_str()).unwrap();
            let destination = CString::new(format!("{}.json", envelope.snapshot_id)).unwrap();
            let linked = unsafe {
                libc::linkat(
                    self.directory.as_raw_fd(),
                    source.as_ptr(),
                    self.directory.as_raw_fd(),
                    destination.as_ptr(),
                    0,
                )
            };
            if linked != 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(io(e));
                }
                // Never overwrite even corrupt existing content at this address.
                self.read(&envelope.snapshot_id, &envelope.content.binding)?;
            }
            Ok(envelope.snapshot_id.clone())
        })();
        let name = CString::new(name).unwrap();
        let unlinked = unsafe { libc::unlinkat(self.directory.as_raw_fd(), name.as_ptr(), 0) };
        check(unlinked == 0, ErrorClass::Io, "STORE_TEMP_CLEANUP_FAILED")?;
        self.directory.sync_all().map_err(io)?;
        operation
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filename_with_embedded_nul_is_rejected_before_creating_a_file() {
        let directory = tempfile::tempdir().unwrap();
        let private = directory.path().join("private-store");
        let store = SnapshotStore::open(&private).unwrap();
        let err = store
            .open_file("pending\0hidden", libc::O_WRONLY | libc::O_CREAT)
            .unwrap_err();
        assert_eq!(err.class, ErrorClass::InvalidInput);
        assert_eq!(err.code, "STORE_FILENAME");
        assert_eq!(std::fs::read_dir(private).unwrap().count(), 0);
    }
}
