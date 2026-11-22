use std::{fs::OpenOptions, io::Read, path::Path};

use super::{Error, config_path};

pub(super) const MAX_KEY_BYTES: usize = 16 * 1024;

pub(super) fn read(path: &Path) -> Result<String, Error> {
    config_path(path)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    let _parents = {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ};
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT).share_mode(FILE_SHARE_READ);
        super::trust_windows::creation_parent(path)?
    };
    let mut file = options.open(path)?;
    let metadata = file.metadata()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(Error::InvalidPublicKey);
        }
    }
    #[cfg(windows)]
    super::trust_windows::check_public_key_file(&file)?;
    if !metadata.is_file() || metadata.len() > MAX_KEY_BYTES as u64 {
        return Err(Error::InvalidPublicKey);
    }
    let mut bytes = vec![0; metadata.len() as usize];
    file.read_exact(&mut bytes)?;
    if file.read(&mut [0])? != 0 {
        return Err(Error::InvalidPublicKey);
    }
    String::from_utf8(bytes).map_err(|_| Error::InvalidPublicKey)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILM+rvN+ot98qgEN796jTiQfZfG1KaT0PtFDJ/XFSqti";

    #[test]
    fn only_bounded_regular_unlinked_files_are_read() {
        let root = tempfile::tempdir().unwrap();
        let key = root.path().join("agent.pub");
        fs::write(&key, KEY).unwrap();
        assert_eq!(read(&key).unwrap(), KEY);
        assert!(read(root.path()).is_err());
        let link = root.path().join("link.pub");
        fs::hard_link(&key, &link).unwrap();
        assert!(matches!(read(&link), Err(Error::InvalidPublicKey)));
        assert!(matches!(read(&key), Err(Error::InvalidPublicKey)));
        fs::remove_file(link).unwrap();
        fs::OpenOptions::new().write(true).open(&key).unwrap().set_len(MAX_KEY_BYTES as u64 + 1).unwrap();
        assert!(matches!(read(&key), Err(Error::InvalidPublicKey)));
    }
}
