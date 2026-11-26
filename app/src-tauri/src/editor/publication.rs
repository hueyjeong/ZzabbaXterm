use std::{
    ffi::OsString,
    fs::File,
    io::{self, Read, Seek},
    path::PathBuf,
};

#[derive(Debug)]
pub(crate) enum PublicationError {
    Io(io::Error),
    Unconfirmed(PathBuf),
}

impl From<io::Error> for PublicationError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

pub(super) fn temporary_name() -> io::Result<OsString> {
    let mut random = [0; 16];
    getrandom::fill(&mut random).map_err(|error| io::Error::other(error.to_string()))?;
    Ok(format!(".zxt-edit-{:032x}.tmp", u128::from_ne_bytes(random)).into())
}

pub(super) fn verify_bytes(mut file: &File, expected: &[u8]) -> io::Result<()> {
    file.rewind()?;
    let mut actual = [0; 8192];
    for chunk in expected.chunks(actual.len()) {
        let read = &mut actual[..chunk.len()];
        file.read_exact(read)?;
        if read != chunk {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "temporary file mismatch",
            ));
        }
    }
    if file.read(&mut actual[..1])? != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "temporary file has trailing bytes",
        ));
    }
    Ok(())
}
