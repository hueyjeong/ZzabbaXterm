use super::CliError;
use std::{fs, io, path::Path};

pub(super) fn validate(executable: &Path) -> Result<(), CliError> {
    #[cfg(windows)]
    let result =
        super::windows_acl::with_install_validator(|check| validate_bundle(executable, check));
    #[cfg(target_os = "linux")]
    let result = validate_bundle(executable, &linux_entry);
    #[cfg(not(any(windows, target_os = "linux")))]
    let result = Err::<(), _>(unsupported());
    result.map_err(|_| CliError::InvalidConfiguration)
}

fn unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "unsupported AWS CLI v2 runtime layout",
    )
}

// AWS CLI v2's aws.spec uses PyInstaller COLLECT and contents_directory='.'.
// The executable's directory is both the native library and Python/data search root.
fn validate_bundle(
    executable: &Path,
    check: &dyn Fn(&Path, bool, bool) -> io::Result<()>,
) -> io::Result<()> {
    let expected = if cfg!(windows) { "aws.exe" } else { "aws" };
    if !executable.is_absolute()
        || !executable
            .file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case(expected))
    {
        return Err(unsupported());
    }
    let root = executable.parent().ok_or_else(unsupported)?;
    for ancestor in root.ancestors() {
        check(ancestor, true, ancestor == root)?;
    }
    check(executable, false, true)?;
    for (marker, directory) in [
        ("base_library.zip", false),
        ("awscli/data", true),
        ("awscli/botocore/data", true),
    ] {
        let kind = root
            .join(marker)
            .symlink_metadata()
            .map_err(|_| unsupported())?
            .file_type();
        if (directory && !kind.is_dir()) || (!directory && !kind.is_file()) {
            return Err(unsupported());
        }
    }

    let mut directories = vec![root.to_path_buf()];
    while let Some(dir) = directories.pop() {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let ty = entry.file_type()?;
            if path == executable {
                continue;
            }
            if ty.is_dir() {
                check(&path, true, true)?;
                directories.push(path);
            } else if ty.is_file() {
                check(&path, false, true)?;
            } else {
                #[cfg(target_os = "linux")]
                if ty.is_symlink() {
                    use std::os::unix::fs::MetadataExt;
                    // PyInstaller onedir can link shared objects to another entry in the same bundle.
                    if path.symlink_metadata()?.uid() == 0 && path.canonicalize()?.starts_with(root)
                    {
                        continue;
                    }
                }
                return Err(unsupported());
            }
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn linux_entry(path: &Path, directory: bool, _: bool) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let meta = path.symlink_metadata()?;
    if meta.uid() != 0
        || meta.mode() & 0o022 != 0
        || (if directory {
            !meta.is_dir()
        } else {
            !meta.is_file()
        })
    {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    Ok(())
}

#[cfg(all(test, windows))]
mod tests {
    use super::super::windows_acl::{private_dir, tests::fixture_dir, with_install_validator};
    use super::*;

    #[test]
    fn onedir_rejects_writable_runtime_files_and_model_directories() {
        let temp = tempfile::tempdir().unwrap();
        let bundle = temp.path().join("bundle");
        private_dir(&bundle).unwrap();
        let exe = bundle.join("aws.exe");
        fs::write(&exe, b"fixture").unwrap();
        let validate = || {
            with_install_validator(|check| {
                // The fixture owns its bundle; host temp ancestors are outside this test.
                validate_bundle(&exe, &|path, dir, add| {
                    if path.starts_with(&bundle) {
                        check(path, dir, add)
                    } else {
                        Ok(())
                    }
                })
            })
        };
        assert_eq!(validate().unwrap_err().kind(), io::ErrorKind::InvalidData);
        fs::write(bundle.join("base_library.zip"), b"fixture").unwrap();
        fs::create_dir_all(bundle.join("awscli/data")).unwrap();
        fs::create_dir_all(bundle.join("awscli/botocore/data")).unwrap();
        fs::write(bundle.join("python314.dll"), b"fixture").unwrap();
        assert!(validate().is_ok());
        let dll = bundle.join("python314.dll");
        super::super::windows_acl::tests::grant_everyone_write(&dll);
        assert_eq!(
            validate().unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        fs::remove_file(dll).unwrap();
        fixture_dir(&bundle.join("awscli/botocore/data/sts"), "GRGWGX");
        assert_eq!(
            validate().unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }
}
