use std::{fs, path::PathBuf};

use bytes::Bytes;
use russh_sftp::{
    de::from_bytes,
    extensions::PosixRenameExtension,
    protocol::{Attrs, Data, File as NameEntry, Handle, Name, Packet, Version},
};
use tokio::io::{self, AsyncReadExt, AsyncWriteExt};

use super::*;
use crate::ssh::{ApprovedHost, Authentication, Endpoint, Transport, TrustStore};

mod pipes;

const DIR: &str = "/workspace";
const FILE: &str = "/workspace/한글.txt";
const ALIAS: &str = "/alias";
const DECOY_DIR: &str = "/other";
const DECOY_FILE: &str = "/other/한글.txt";
const REDIRECT: &str = "redirect";
const ORIGINAL: &str = "처음 글\r\n";
const EXTERNAL: &str = "외부 글\r\n";
const EDITED: &str = "내 수정\r\n";
const UNRELATED: &str = ".zzabba-edit-other";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Scenario {
    Normal,
    ExternalEdit,
    ExternalDelete,
    ChangedHandleMetadata,
    ChangedTargetDuringRead,
    NoExtension,
    OtherExtensionVersion,
    CreateRefused,
    CreateReplyLost,
    PublicTemporaryFile,
    WriteRefused,
    WriteDisconnected,
    CloseRefused,
    RenameRefused,
    RenameReplyLost,
}

struct Fixture {
    root: PathBuf,
    scenario: Scenario,
    temp: Option<String>,
    mode: u32,
    temp_mode: u32,
    write_count: usize,
    directory_read: bool,
    upload_closed: bool,
    original_rechecked: bool,
    read_opens: usize,
    reached_eof: bool,
    removed: bool,
    renamed: bool,
}

impl Fixture {
    fn new(root: PathBuf, scenario: Scenario) -> Self {
        fs::write(root.join("original"), ORIGINAL).unwrap();
        fs::write(root.join("decoy"), EXTERNAL).unwrap();
        fs::write(root.join(UNRELATED), "별도 파일").unwrap();
        Self {
            root,
            scenario,
            temp: None,
            mode: 0o640,
            temp_mode: 0,
            write_count: 0,
            directory_read: false,
            upload_closed: false,
            original_rechecked: false,
            read_opens: 0,
            reached_eof: false,
            removed: false,
            renamed: false,
        }
    }

    fn attrs(&self, name: &str, mode: u32) -> FileAttributes {
        FileAttributes {
            permissions: Some(0o100000 | mode),
            uid: Some(1001),
            gid: Some(1002),
            size: Some(fs::metadata(self.root.join(name)).unwrap().len()),
            atime: Some(123),
            mtime: Some(123),
            ..FileAttributes::empty()
        }
    }

    fn original_attrs(&self) -> FileAttributes {
        self.attrs("original", self.mode)
    }

    fn respond(&mut self, packet: Packet) -> Option<Packet> {
        let id = packet.get_request_id();
        let ok = || Packet::status(id, StatusCode::Ok, "", "");
        let handle = |bytes: &[u8]| {
            Handle {
                id,
                handle: bytes.into(),
            }
            .into()
        };
        Some(match packet {
            Packet::Init(init) => {
                assert_eq!(init.version, 3);
                let mut version = Version::new();
                if self.scenario != Scenario::NoExtension {
                    let value = if self.scenario == Scenario::OtherExtensionVersion {
                        "2"
                    } else {
                        "1"
                    };
                    version.extensions.insert(POSIX_RENAME.into(), value.into());
                }
                version.into()
            }
            Packet::RealPath(request) => {
                let redirected = self.root.join(REDIRECT).exists();
                let resolved = match request.path.to_str().unwrap() {
                    ALIAS if redirected => DECOY_DIR,
                    FILE if self.scenario == Scenario::ChangedTargetDuringRead
                        && self.reached_eof =>
                    {
                        DECOY_FILE
                    }
                    "/" => "/",
                    DIR | "." | ALIAS => DIR,
                    FILE => FILE,
                    DECOY_DIR => DECOY_DIR,
                    DECOY_FILE => DECOY_FILE,
                    path => panic!("예상하지 않은 경로: {path}"),
                };
                Name {
                    id,
                    files: vec![NameEntry::dummy(resolved)],
                }
                .into()
            }
            Packet::OpenDir(request) => {
                self.directory_read = false;
                match request.path.to_str().unwrap() {
                    "/" => handle(b"root"),
                    DIR => handle(b"dir\xff"),
                    DECOY_DIR => handle(b"decoy-dir"),
                    path => panic!("예상하지 않은 디렉터리: {path}"),
                }
            }
            Packet::ReadDir(request) => {
                if request.handle.as_bytes() == b"root" {
                    if self.directory_read {
                        return Some(Packet::error(id, StatusCode::Eof));
                    }
                    self.directory_read = true;
                    let mut directory = NameEntry::dummy("workspace");
                    directory.attrs.permissions = Some(0o040755);
                    return Some(
                        Name {
                            id,
                            files: vec![directory],
                        }
                        .into(),
                    );
                }
                let decoy = request.handle.as_bytes() == b"decoy-dir";
                assert!(decoy || request.handle.as_bytes() == b"dir\xff");
                if self.directory_read {
                    Packet::error(id, StatusCode::Eof)
                } else {
                    self.directory_read = true;
                    let mut regular = NameEntry::dummy("한글.txt");
                    regular.attrs = if decoy {
                        self.attrs("decoy", self.mode)
                    } else {
                        self.original_attrs()
                    };
                    let mut traversal = regular.clone();
                    traversal.filename = "../outside".into();
                    let mut temporary = regular.clone();
                    temporary.filename = UNRELATED.into();
                    let mut non_utf8 = regular.clone();
                    non_utf8.filename = b"\xff".as_slice().into();
                    Name {
                        id,
                        files: vec![
                            NameEntry::dummy("."),
                            regular,
                            traversal,
                            temporary,
                            non_utf8,
                        ],
                    }
                    .into()
                }
            }
            Packet::Lstat(request) => {
                let name = match request.path.to_str().unwrap() {
                    FILE => "original",
                    DECOY_FILE => "decoy",
                    path => panic!("예상하지 않은 파일: {path}"),
                };
                if !self.root.join(name).exists() {
                    Packet::error(id, StatusCode::NoSuchFile)
                } else {
                    Attrs {
                        id,
                        attrs: self.attrs(name, self.mode),
                    }
                    .into()
                }
            }
            Packet::Open(request) if request.pflags.bits() == OpenFlags::READ.bits() => {
                self.read_opens += 1;
                self.reached_eof = false;
                match request.filename.to_str().unwrap() {
                    FILE => handle(b"original\0"),
                    DECOY_FILE => handle(b"decoy"),
                    path => panic!("예상하지 않은 파일: {path}"),
                }
            }
            Packet::Open(request) => {
                assert_eq!(
                    request.pflags.bits(),
                    (OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::EXCLUDE).bits()
                );
                assert_eq!(request.attrs.permissions, Some(0o600));
                let path = request.filename.into_string().unwrap();
                let (parent, name) = path.rsplit_once('/').unwrap();
                assert_eq!(parent, DIR);
                assert!(name.starts_with(".zzabba-edit-"));
                if self.scenario == Scenario::CreateRefused {
                    fs::write(self.root.join(name), "이미 존재하는 파일").unwrap();
                    self.temp = Some(name.into());
                    return Some(Packet::error(id, StatusCode::Failure));
                }
                fs::File::create_new(self.root.join(name)).unwrap();
                self.temp = Some(name.into());
                self.temp_mode = if self.scenario == Scenario::PublicTemporaryFile {
                    0o644
                } else {
                    0o600
                };
                self.upload_closed = false;
                self.original_rechecked = false;
                if self.scenario == Scenario::CreateReplyLost {
                    return None;
                }
                handle(b"tmp\0")
            }
            Packet::Fstat(request) => {
                let mut attrs = match request.handle.as_bytes() {
                    b"tmp\0" => self.attrs(self.temp.as_deref().unwrap(), self.temp_mode),
                    b"decoy" => self.attrs("decoy", self.mode),
                    b"original\0" => self.original_attrs(),
                    other => panic!("예상하지 않은 핸들: {other:?}"),
                };
                if request.handle.as_bytes() == b"original\0"
                    && self.scenario == Scenario::ChangedHandleMetadata
                    && self.reached_eof
                {
                    attrs.mtime = Some(124);
                }
                Attrs { id, attrs }.into()
            }
            Packet::Read(request) => {
                let name = match request.handle.as_bytes() {
                    b"original\0" => "original",
                    b"decoy" => "decoy",
                    other => panic!("예상하지 않은 핸들: {other:?}"),
                };
                let contents = fs::read(self.root.join(name)).unwrap();
                let offset = request.offset as usize;
                if offset >= contents.len() {
                    self.reached_eof = true;
                    if self.upload_closed {
                        self.original_rechecked = true;
                    }
                    Packet::error(id, StatusCode::Eof)
                } else {
                    // 짧은 DATA를 보내고 소비자가 실제 받은 길이만큼 이동하는지 확인한다.
                    let chunk_size = if contents.len() < CHUNK { 3 } else { CHUNK };
                    let end = contents
                        .len()
                        .min(offset + chunk_size)
                        .min(offset + request.len as usize);
                    Data {
                        id,
                        data: contents[offset..end].into(),
                    }
                    .into()
                }
            }
            Packet::Write(request) => {
                use std::io::{Seek, SeekFrom, Write};
                assert_eq!(request.handle.as_bytes(), b"tmp\0");
                self.write_count += 1;
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .open(self.root.join(self.temp.as_ref().unwrap()))
                    .unwrap();
                assert_eq!(request.offset, file.metadata().unwrap().len());
                file.seek(SeekFrom::Start(request.offset)).unwrap();
                if self.write_count == 2
                    && matches!(
                        self.scenario,
                        Scenario::WriteRefused | Scenario::WriteDisconnected
                    )
                {
                    file.write_all(&request.data[..1]).unwrap();
                    if self.scenario == Scenario::WriteDisconnected {
                        return None;
                    }
                    Packet::error(id, StatusCode::Failure)
                } else {
                    file.write_all(&request.data).unwrap();
                    ok()
                }
            }
            Packet::FSetStat(request) => {
                assert_eq!(request.handle.as_bytes(), b"tmp\0");
                self.temp_mode = request.attrs.permissions.unwrap();
                ok()
            }
            Packet::Close(request) => {
                if request.handle.as_bytes() == b"tmp\0" {
                    if self.scenario == Scenario::CloseRefused {
                        return Some(Packet::error(id, StatusCode::Failure));
                    }
                    self.upload_closed = true;
                    match self.scenario {
                        Scenario::ExternalEdit => {
                            assert_eq!(ORIGINAL.len(), EXTERNAL.len());
                            fs::write(self.root.join("original"), EXTERNAL).unwrap();
                        }
                        Scenario::ExternalDelete => {
                            fs::remove_file(self.root.join("original")).unwrap()
                        }
                        _ => (),
                    }
                }
                ok()
            }
            Packet::Remove(request) => {
                let name = self.temp.take().unwrap();
                assert_eq!(request.filename, child_path(DIR, &name).as_str());
                self.removed = true;
                fs::remove_file(self.root.join(name)).unwrap();
                ok()
            }
            Packet::Extended(request) => {
                assert_eq!(request.request, POSIX_RENAME);
                let rename: PosixRenameExtension =
                    from_bytes(&mut Bytes::from(request.data)).unwrap();
                let name = self.temp.as_ref().unwrap();
                assert_eq!(rename.oldpath, child_path(DIR, name).as_str());
                assert_eq!(rename.newpath, FILE);
                assert!(self.upload_closed && self.original_rechecked);
                assert_eq!(self.temp_mode, self.mode);
                if self.scenario == Scenario::RenameRefused {
                    return Some(Packet::error(id, StatusCode::OpUnsupported));
                }
                fs::rename(
                    self.root.join(self.temp.take().unwrap()),
                    self.root.join("original"),
                )
                .unwrap();
                self.renamed = true;
                if self.scenario == Scenario::RenameReplyLost {
                    return None;
                }
                ok()
            }
            other => panic!("예상하지 않은 요청: {other:?}"),
        })
    }

    async fn run(mut self, mut stream: impl io::AsyncRead + io::AsyncWrite + Unpin) -> Self {
        loop {
            let len = match stream.read_u32().await {
                Ok(len) => len,
                Err(error) if matches!(error.kind(), std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset) => break,
                Err(error) => panic!("{error}"),
            };
            let mut bytes = vec![0; len as usize];
            stream.read_exact(&mut bytes).await.unwrap();
            let packet = Packet::try_from(&mut Bytes::from(bytes)).unwrap();
            let Some(response) = self.respond(packet) else {
                break;
            };
            stream
                .write_all(&Bytes::try_from(response).unwrap())
                .await
                .unwrap();
        }
        self
    }
}

fn plan(root: &std::path::Path, account: &str) -> Arc<ConnectionPlan> {
    let store = TrustStore::create(root).unwrap();
    let endpoint = Endpoint::new("editor.example.invalid", 22).unwrap();
    let approved = ApprovedHost::confirm(
        &endpoint,
        &endpoint,
        "SHA256:UCUiLr7Pjs9wFFJMDByLgc3NrtdU344OgUM45wZPcIQ",
    )
    .unwrap();
    store
        .register(
            &approved,
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILM+rvN+ot98qgEN796jTiQfZfG1KaT0PtFDJ/XFSqti",
        )
        .unwrap();
    Arc::new(
        ConnectionPlan::prepare(
            &store,
            &approved,
            account,
            Transport::Direct,
            Authentication::ManualPassword,
        )
        .unwrap(),
    )
}

async fn connect(
    plan: Arc<ConnectionPlan>,
    fixture: Fixture,
) -> (Connection, tokio::task::JoinHandle<Fixture>) {
    let (client, server) = io::duplex(64 * 1024);
    let server = tokio::spawn(fixture.run(server));
    let (stdout, stdin) = io::split(client);
    (
        Connection::from_subsystem(plan, stdin, stdout, 1)
            .await
            .unwrap(),
        server,
    )
}

async fn listed_file(connection: &mut Connection) -> Entry {
    connection.list(None).await.unwrap().pop().unwrap()
}

#[tokio::test]
async fn lists_reads_and_replaces_with_permissions_then_saves_again() {
    let root = tempfile::tempdir().unwrap();
    let plan = plan(&root.path().join("trust"), "editor");
    let fixture = Fixture::new(root.path().to_owned(), Scenario::Normal);
    let (mut connection, server) = connect(plan, fixture).await;
    let root_entries = connection.list(Some("/")).await.unwrap();
    let directory = &root_entries[0];
    assert!(directory.is_directory());
    let entries = connection.list_directory(directory).await.unwrap();
    assert_eq!(
        entries
            .iter()
            .map(|entry| (entry.name(), entry.is_directory()))
            .collect::<Vec<_>>(),
        [("한글.txt", false)]
    );
    let (mut file, text) = connection.open(&entries[0]).await.unwrap();
    assert_eq!(text, ORIGINAL);
    let large = "한\r\n".repeat(CHUNK / 2);
    assert!(matches!(
        connection.save(&mut file, &large).await,
        SaveOutcome::Saved
    ));
    assert!(matches!(
        connection.save(&mut file, EDITED).await,
        SaveOutcome::Saved
    ));
    assert_eq!(connection.reopen(&file).await.unwrap().1, EDITED);
    drop(connection);
    let fixture = server.await.unwrap();
    assert!(fixture.renamed && fixture.temp.is_none());
    assert_eq!(
        fs::read_to_string(root.path().join(UNRELATED)).unwrap(),
        "별도 파일"
    );
}

#[tokio::test]
async fn post_close_external_edits_and_deletion_keep_the_edit_buffer() {
    for scenario in [Scenario::ExternalEdit, Scenario::ExternalDelete] {
        let root = tempfile::tempdir().unwrap();
        let plan = plan(&root.path().join("trust"), "editor");
        let fixture = Fixture::new(root.path().to_owned(), scenario);
        let (mut connection, server) = connect(plan, fixture).await;
        let entry = listed_file(&mut connection).await;
        let (mut file, _) = connection.open(&entry).await.unwrap();
        let edited = EDITED.to_owned();
        let result = connection.save(&mut file, &edited).await;
        match (scenario, result) {
            (
                Scenario::ExternalEdit,
                SaveOutcome::Conflict {
                    remote: RemoteVersion::Text(remote),
                    cleanup: Cleanup::Removed,
                },
            ) => assert_eq!(remote, EXTERNAL),
            (
                Scenario::ExternalDelete,
                SaveOutcome::Conflict {
                    remote: RemoteVersion::Missing,
                    cleanup: Cleanup::Removed,
                },
            ) => (),
            (_, other) => panic!("예상하지 않은 충돌 결과: {other:?}"),
        }
        assert_eq!(edited, EDITED);
        drop(connection);
        let fixture = server.await.unwrap();
        assert!(!fixture.renamed && fixture.removed && fixture.temp.is_none());
        let original = root.path().join("original");
        if scenario == Scenario::ExternalEdit {
            assert_eq!(fs::read_to_string(original).unwrap(), EXTERNAL);
        } else {
            assert!(!original.exists());
        }
    }
}

#[tokio::test]
async fn refuses_cross_account_and_only_uses_the_supported_extension_version() {
    let root = tempfile::tempdir().unwrap();
    let first_plan = plan(&root.path().join("first"), "editor");
    let second_plan = plan(&root.path().join("second"), "other");
    let (mut first, first_server) = connect(
        first_plan.clone(),
        Fixture::new(root.path().to_owned(), Scenario::Normal),
    )
    .await;
    let entry = listed_file(&mut first).await;
    let (mut file, _) = first.open(&entry).await.unwrap();
    drop(first);
    assert!(first_server.await.unwrap().temp.is_none());

    let (mut other, other_server) = connect(
        second_plan,
        Fixture::new(root.path().to_owned(), Scenario::Normal),
    )
    .await;
    assert!(matches!(
        other.open(&entry).await,
        Err(Error::WrongConnectionPlan)
    ));
    assert!(matches!(
        other.save(&mut file, EDITED).await,
        SaveOutcome::Failed {
            stage: SaveStage::Prepare,
            error: Error::WrongConnectionPlan,
            cleanup: None
        }
    ));
    drop(other);
    assert!(other_server.await.unwrap().temp.is_none());

    for scenario in [Scenario::NoExtension, Scenario::OtherExtensionVersion] {
        let (mut connection, server) = connect(
            first_plan.clone(),
            Fixture::new(root.path().to_owned(), scenario),
        )
        .await;
        assert!(matches!(
            connection.save(&mut file, EDITED).await,
            SaveOutcome::Failed {
                stage: SaveStage::Prepare,
                error: Error::AtomicReplaceUnavailable,
                cleanup: None
            }
        ));
        drop(connection);
        assert!(server.await.unwrap().temp.is_none());
        assert_eq!(
            fs::read_to_string(root.path().join("original")).unwrap(),
            ORIGINAL
        );
    }
}

#[tokio::test]
async fn close_and_partial_write_failures_never_replace_the_original() {
    for scenario in [
        Scenario::CreateRefused,
        Scenario::CreateReplyLost,
        Scenario::PublicTemporaryFile,
        Scenario::WriteRefused,
        Scenario::WriteDisconnected,
        Scenario::CloseRefused,
        Scenario::RenameRefused,
    ] {
        let root = tempfile::tempdir().unwrap();
        let plan = plan(&root.path().join("trust"), "editor");
        let fixture = Fixture::new(root.path().to_owned(), scenario);
        let (mut connection, server) = connect(plan, fixture).await;
        let entry = listed_file(&mut connection).await;
        let (mut file, _) = connection.open(&entry).await.unwrap();
        let edited = "편집 내용".repeat(CHUNK / 2);
        let result = connection.save(&mut file, &edited).await;
        match (scenario, result) {
            (
                Scenario::CreateRefused | Scenario::CreateReplyLost,
                SaveOutcome::Failed {
                    stage: SaveStage::Create,
                    cleanup: None,
                    ..
                },
            ) => (),
            (
                Scenario::PublicTemporaryFile,
                SaveOutcome::Failed {
                    stage: SaveStage::Permissions,
                    error: Error::UnsafePermissions,
                    cleanup: Some(Cleanup::Removed),
                },
            ) => (),
            (
                Scenario::WriteRefused,
                SaveOutcome::Failed {
                    stage: SaveStage::Write,
                    cleanup: Some(Cleanup::Removed),
                    ..
                },
            ) => (),
            (
                Scenario::WriteDisconnected,
                SaveOutcome::Failed {
                    stage: SaveStage::Write,
                    cleanup: Some(Cleanup::Failed(_)),
                    ..
                },
            ) => (),
            (
                Scenario::CloseRefused,
                SaveOutcome::Failed {
                    stage: SaveStage::Close,
                    cleanup: Some(Cleanup::Removed),
                    ..
                },
            ) => (),
            (
                Scenario::RenameRefused,
                SaveOutcome::Failed {
                    stage: SaveStage::Replace,
                    error: Error::AtomicReplaceUnavailable,
                    cleanup: Some(Cleanup::Removed),
                },
            ) => (),
            (_, other) => panic!("예상하지 않은 실패 결과: {other:?}"),
        }
        assert_eq!(
            fs::read_to_string(root.path().join("original")).unwrap(),
            ORIGINAL
        );
        drop(connection);
        let fixture = server.await.unwrap();
        assert!(!fixture.renamed);
        if scenario == Scenario::PublicTemporaryFile {
            assert_eq!(fixture.write_count, 0);
        }
        match scenario {
            Scenario::CreateRefused => assert_eq!(
                fs::read_to_string(root.path().join(fixture.temp.unwrap())).unwrap(),
                "이미 존재하는 파일"
            ),
            Scenario::CreateReplyLost => assert!(root.path().join(fixture.temp.unwrap()).exists()),
            Scenario::WriteDisconnected => assert_eq!(
                fs::metadata(root.path().join(fixture.temp.unwrap()))
                    .unwrap()
                    .len(),
                CHUNK as u64 + 1
            ),
            _ => assert!(fixture.removed && fixture.temp.is_none()),
        }
        assert_eq!(
            fs::read_to_string(root.path().join(UNRELATED)).unwrap(),
            "별도 파일"
        );
    }
}

#[tokio::test]
async fn lost_replace_response_requires_reopening_even_when_replacement_happened() {
    let root = tempfile::tempdir().unwrap();
    let plan = plan(&root.path().join("trust"), "editor");
    let fixture = Fixture::new(root.path().to_owned(), Scenario::RenameReplyLost);
    let (mut connection, server) = connect(plan.clone(), fixture).await;
    let entry = listed_file(&mut connection).await;
    let (mut file, _) = connection.open(&entry).await.unwrap();
    assert!(matches!(
        connection.save(&mut file, EDITED).await,
        SaveOutcome::Unconfirmed(_)
    ));
    assert!(matches!(
        connection.save(&mut file, EDITED).await,
        SaveOutcome::Failed {
            stage: SaveStage::Prepare,
            error: Error::ReopenRequired,
            cleanup: None
        }
    ));
    drop(connection);
    let mut fixture = server.await.unwrap();
    assert!(fixture.renamed && !fixture.removed);
    assert_eq!(
        fs::read_to_string(root.path().join("original")).unwrap(),
        EDITED
    );
    fixture.scenario = Scenario::Normal;
    fixture.renamed = false;
    let (mut reconnected, server) = connect(plan, fixture).await;
    let (mut refreshed, text) = reconnected.reopen(&file).await.unwrap();
    assert_eq!(text, EDITED);
    assert!(matches!(
        reconnected.save(&mut refreshed, ORIGINAL).await,
        SaveOutcome::Saved
    ));
    drop(reconnected);
    assert!(server.await.unwrap().renamed);
}

#[tokio::test]
async fn listed_alias_cannot_retarget_to_a_same_named_file() {
    let root = tempfile::tempdir().unwrap();
    let plan = plan(&root.path().join("trust"), "editor");
    let fixture = Fixture::new(root.path().to_owned(), Scenario::Normal);
    let (mut connection, server) = connect(plan, fixture).await;
    let entry = connection.list(Some(ALIAS)).await.unwrap().pop().unwrap();
    let (mut file, text) = connection.open(&entry).await.unwrap();
    assert_eq!(text, ORIGINAL);
    fs::write(root.path().join(REDIRECT), []).unwrap();
    assert!(matches!(
        connection.open(&entry).await,
        Err(Error::PathChanged)
    ));
    assert!(matches!(
        connection.reopen(&file).await,
        Err(Error::PathChanged)
    ));
    assert!(matches!(
        connection.save(&mut file, EDITED).await,
        SaveOutcome::Failed {
            stage: SaveStage::Prepare,
            error: Error::PathChanged,
            cleanup: None
        }
    ));
    let new_entry = connection.list(Some(ALIAS)).await.unwrap().pop().unwrap();
    assert_eq!(entry.name(), new_entry.name());
    assert_eq!(connection.open(&new_entry).await.unwrap().1, EXTERNAL);
    drop(connection);
    let fixture = server.await.unwrap();
    assert_eq!(fixture.read_opens, 2);
    assert_eq!(fixture.write_count, 0);
}

#[tokio::test]
async fn rejects_changes_observed_after_reading_the_handle() {
    for scenario in [
        Scenario::ChangedHandleMetadata,
        Scenario::ChangedTargetDuringRead,
    ] {
        let root = tempfile::tempdir().unwrap();
        let plan = plan(&root.path().join("trust"), "editor");
        let fixture = Fixture::new(root.path().to_owned(), scenario);
        let (mut connection, server) = connect(plan, fixture).await;
        let entry = listed_file(&mut connection).await;
        match (scenario, connection.open(&entry).await) {
            (Scenario::ChangedHandleMetadata, Err(Error::ReadChanged))
            | (Scenario::ChangedTargetDuringRead, Err(Error::PathChanged)) => (),
            _ => panic!("읽기 도중 경로나 속성이 바뀐 파일을 거절해야 한다"),
        }
        drop(connection);
        let fixture = server.await.unwrap();
        assert!(fixture.reached_eof);
        assert_eq!(fixture.read_opens, 1);
    }
}
