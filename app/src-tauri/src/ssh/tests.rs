use std::process::Command;

use super::*;

const KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILM+rvN+ot98qgEN796jTiQfZfG1KaT0PtFDJ/XFSqti";
const SHA: &str = "SHA256:UCUiLr7Pjs9wFFJMDByLgc3NrtdU344OgUM45wZPcIQ";
const OTHER_KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIC43GzF6fNOSizxOL5WTkKKSfu0OGcQymwLyXHRmq8Pm";
const OTHER_SHA: &str = "SHA256:k/KBYp42xBgn72aCjefPr1hQdnwjGXOW2zC2gDA9ehw";

fn setup() -> (SessionFiles, TrustStore, Endpoint) {
    let root = session_files(&std::env::temp_dir()).unwrap();
    let store = TrustStore::open(root.path()).unwrap();
    (root, store, Endpoint::new("Dev.Example.invalid", 2222).unwrap())
}

fn approve<'a>(endpoint: &'a Endpoint, sha: &str) -> ApprovedHost<'a> {
    ApprovedHost::confirm(endpoint, endpoint, sha).unwrap()
}

#[test]
fn f01_only_matching_sha_creates_a_pin_and_registration_never_replaces_it() {
    let (root, store, endpoint) = setup();
    for (key, sha) in [(KEY, OTHER_SHA), (OTHER_KEY, SHA)] {
        assert!(matches!(store.register(&approve(&endpoint, sha), key), Err(Error::FingerprintMismatch)));
    }
    let multiline = format!("{KEY}\n{OTHER_KEY}");
    let approved = approve(&endpoint, SHA);
    assert!(matches!(store.register(&approved, &multiline), Err(Error::InvalidPublicKey)));
    let wrong_algorithm = KEY.replace("ssh-ed25519", "ssh-rsa");
    assert!(matches!(store.register(&approved, &wrong_algorithm), Err(Error::InvalidPublicKey)));
    assert!(root.path().read_dir().unwrap().next().is_none());
    store.register(&approved, &format!("{KEY} administrator\r\n")).unwrap();
    store.register(&approved, KEY).unwrap();
    assert!(matches!(store.register(&approve(&endpoint, OTHER_SHA), OTHER_KEY), Err(Error::ExistingPinConflict)));
    let record = store.pin(&approved).unwrap().0;
    assert_eq!(record, format!("[dev.example.invalid]:2222 {KEY}\n"));

    let program = openssh_directory().unwrap().join(if cfg!(windows) { "ssh-keygen.exe" } else { "ssh-keygen" });
    let fixture = root.path().join("fixed public key.pub");
    fs::write(&fixture, format!("{KEY}\n")).unwrap();
    let output = Command::new(program).args(["-l", "-E", "sha256", "-f"]).arg(fixture).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).split_ascii_whitespace().nth(1), Some(SHA));
}

#[test]
fn f01_both_transports_require_the_exact_host_and_port_before_planning() {
    let (_root, store, endpoint) = setup();
    let approved = approve(&endpoint, SHA);
    store.register(&approved, KEY).unwrap();
    for (host, port) in [("dev.example.invalid", 22), ("other.example.invalid", 2222), ("dev.example.invalid.", 2222)] {
        let other = Endpoint::new(host, port).unwrap();
        assert!(matches!(ApprovedHost::confirm(&other, &endpoint, SHA), Err(Error::ApprovedEndpointMismatch)));
        let other = approve(&other, SHA);
        for transport in [Transport::Direct, Transport::Ssm] {
            assert!(matches!(
                ConnectionPlan::prepare(&store, &other, "dev", transport, Authentication::ManualPassword),
                Err(Error::UnregisteredHost),
            ));
        }
    }
    assert!(matches!(
        ConnectionPlan::prepare(&store, &approved, "dev", Transport::Ssm, Authentication::Agent { public_key: KEY }),
        Err(Error::SsmProxyUnavailable),
    ));
    assert!(matches!(
        ConnectionPlan::prepare(&store, &approved, "dev", Transport::Direct, Authentication::StoredPassword),
        Err(Error::StoredAuthenticationUnavailable),
    ));
}

#[test]
fn f01_corrupt_pin_never_produces_an_invocation_or_gets_replaced() {
    let (root, store, endpoint) = setup();
    let approved = approve(&endpoint, SHA);
    store.register(&approved, KEY).unwrap();
    let path = root.path().read_dir().unwrap().next().unwrap().unwrap().path();
    let corrupt = format!("[somewhere-else]:2222 {KEY}\n");
    fs::write(&path, &corrupt).unwrap();
    assert!(matches!(
        ConnectionPlan::prepare(&store, &approved, "dev", Transport::Direct, Authentication::ManualPassword),
        Err(Error::InvalidTrustRecord),
    ));
    assert!(matches!(store.register(&approved, KEY), Err(Error::ExistingPinConflict)));
    assert_eq!(fs::read_to_string(&path).unwrap(), corrupt);
    let changed = format!("{} {OTHER_KEY}\n", endpoint.alias());
    fs::write(&path, &changed).unwrap();
    for transport in [Transport::Direct, Transport::Ssm] {
        assert!(matches!(
            ConnectionPlan::prepare(&store, &approved, "dev", transport, Authentication::ManualPassword),
            Err(Error::FingerprintMismatch),
        ));
    }
    assert!(matches!(store.register(&approved, KEY), Err(Error::ExistingPinConflict)));
    assert_eq!(fs::read_to_string(path).unwrap(), changed);
}

#[test]
fn f01_shared_trust_root_is_rejected_at_open_and_use() {
    let (root, store, endpoint) = setup();
    let approved = approve(&endpoint, SHA);
    store.register(&approved, KEY).unwrap();
    share_for_test(root.path());
    assert!(matches!(TrustStore::open(root.path()), Err(Error::InvalidPath)));
    assert!(matches!(store.register(&approved, KEY), Err(Error::InvalidPath)));
    assert!(matches!(store.pin(&approved), Err(Error::InvalidPath)));
}

#[test]
fn f01_shared_or_hard_linked_pin_is_rejected() {
    let (root, store, endpoint) = setup();
    let approved = approve(&endpoint, SHA);
    store.register(&approved, KEY).unwrap();
    let path = root.path().read_dir().unwrap().next().unwrap().unwrap().path();
    let link = root.path().join("second link");
    fs::hard_link(&path, &link).unwrap();
    assert!(matches!(store.pin(&approved), Err(Error::InvalidTrustRecord)));
    fs::remove_file(link).unwrap();
    share_for_test(&path);
    assert!(matches!(store.pin(&approved), Err(Error::InvalidTrustRecord)));
    assert!(matches!(store.register(&approved, KEY), Err(Error::InvalidTrustRecord)));
}

#[test]
fn f01_replaced_root_with_private_permissions_is_rejected() {
    let (parent, _, endpoint) = setup();
    let root = parent.path().join("original");
    let replacement = parent.path().join("replacement");
    for path in [&root, &replacement] {
        #[cfg(windows)]
        trust_windows::create_dir(path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            fs::DirBuilder::new().mode(0o700).create(path).unwrap();
        }
    }
    let store = TrustStore::open(&root).unwrap();
    let approved = approve(&endpoint, SHA);
    fs::rename(&root, parent.path().join("moved")).unwrap();
    fs::rename(replacement, &root).unwrap();
    assert!(matches!(store.register(&approved, KEY), Err(Error::TrustRootChanged)));
    assert!(matches!(store.pin(&approved), Err(Error::TrustRootChanged)));
    assert!(root.read_dir().unwrap().next().is_none());
}

fn share_for_test(path: &Path) {
    #[cfg(windows)]
    trust_windows::set_test_acl(path, true, true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o777)).unwrap();
    }
}

#[cfg(windows)]
#[test]
fn f01_protected_acl_survives_parent_acl_changes() {
    let parent = session_files(&std::env::temp_dir()).unwrap();
    trust_windows::set_test_acl(parent.path(), true, true);
    let root = parent.path().join("app trust");
    let store = TrustStore::create(&root).unwrap();
    assert!(matches!(TrustStore::create(&root), Err(Error::Io(error)) if error.kind() == io::ErrorKind::AlreadyExists));
    assert!(matches!(TrustStore::open(parent.path()), Err(Error::InvalidPath)));

    let endpoint = Endpoint::new("dev.example.invalid", 22).unwrap();
    let approved = approve(&endpoint, SHA);
    store.register(&approved, KEY).unwrap();
    let pin = root.read_dir().unwrap().next().unwrap().unwrap().path();
    let plan = ConnectionPlan::prepare(&store, &approved, "dev", Transport::Direct,
        Authentication::Agent { public_key: KEY }).unwrap();
    let session = plan.config.parent().unwrap();
    trust_windows::set_test_acl(parent.path(), true, false);
    trust_windows::set_test_acl(parent.path(), true, true);
    assert!(store.pin(&approved).is_ok());
    assert!(trust_windows::root(session).is_ok());
    for path in [&pin, &plan.config, &session.join("known_hosts"), &session.join("agent.pub")] {
        assert!(trust_windows::pin(path).is_ok(), "{}", path.display());
    }
}

#[cfg(windows)]
#[test]
fn f01_private_but_unprotected_acl_blocks_preparation() {
    let (root, store, endpoint) = setup();
    let approved = approve(&endpoint, SHA);
    store.register(&approved, KEY).unwrap();
    let pin = root.path().read_dir().unwrap().next().unwrap().unwrap().path();
    trust_windows::set_test_acl(&pin, false, false);
    assert!(matches!(ConnectionPlan::prepare(&store, &approved, "dev", Transport::Direct,
        Authentication::ManualPassword), Err(Error::UnprotectedTrustAcl)));
    trust_windows::set_test_acl(&pin, true, false);
    trust_windows::set_test_acl(root.path(), false, false);
    assert!(matches!(TrustStore::open(root.path()), Err(Error::UnprotectedTrustAcl)));
    assert!(matches!(store.register(&approved, KEY), Err(Error::UnprotectedTrustAcl)));
    assert!(matches!(ConnectionPlan::prepare(&store, &approved, "dev", Transport::Direct,
        Authentication::ManualPassword), Err(Error::UnprotectedTrustAcl)));
}

#[test]
fn f01_ssh_and_sftp_use_one_pinned_config_with_exclusive_authentication() {
    let (root, store, endpoint) = setup();
    let approved = approve(&endpoint, SHA);
    store.register(&approved, KEY).unwrap();
    let key_path = root.path().join("user's selected private key");
    for (auth, preferred, public_key, password, batch) in [
        (Authentication::Agent { public_key: KEY }, "publickey", "yes", "no", "yes"),
        (Authentication::KeyFile { private_key: &key_path }, "publickey", "yes", "no", "no"),
        (Authentication::ManualPassword, "password", "no", "yes", "no"),
    ] {
        let plan = ConnectionPlan::prepare(&store, &approved, "test-user", Transport::Direct, auth).unwrap();
        let ssh = plan.invocation(Client::Ssh);
        let sftp = plan.invocation(Client::Sftp);
        assert!(ssh.program.is_absolute() && ssh.program.is_file());
        assert!(sftp.program.is_absolute() && sftp.program.is_file());
        assert_eq!(sftp.program.file_stem().unwrap(), "sftp");
        assert_eq!(ssh.program.file_stem().unwrap(), "ssh");
        assert_eq!(ssh.args, [OsStr::new("-F"), plan.config.as_os_str(), OsStr::new("--"), OsStr::new("zzabba-target")]);
        assert_eq!(sftp.args[..2], [OsStr::new("-S"), ssh.program.as_os_str()]);
        assert_eq!(sftp.args[2..], ssh.args);
        assert_eq!(sftp.environment, ssh.environment);
        assert_eq!(ssh.environment, [("SSH_ASKPASS", None), ("SSH_ASKPASS_REQUIRE", Some("never"))]);

        // -G는 연결하지 않는다. 다른 대상 인자도 이 계획의 설정·신뢰 경계를 벗어날 수 없다.
        let output = Command::new(ssh.program).arg("-G").args(&ssh.args[..3])
            .arg("unrelated.invalid").env_remove("SSH_AUTH_SOCK").output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let effective = String::from_utf8(output.stdout).unwrap();
        let values = |name| effective.lines().filter_map(move |line| {
            let (key, value) = line.split_once(' ')?;
            (key == name).then_some(value)
        }).collect::<Vec<_>>();
        for (option, expected) in [
            ("hostname", "dev.example.invalid"), ("port", "2222"), ("user", "test-user"),
            ("hostkeyalias", "[dev.example.invalid]:2222"), ("stricthostkeychecking", "yes"),
            ("verifyhostkeydns", "no"), ("checkhostip", "no"), ("updatehostkeys", "no"),
            ("hostkeyalgorithms", "ssh-ed25519"), ("forwardagent", "no"), ("identitiesonly", "yes"),
            ("kbdinteractiveauthentication", "no"), ("hostbasedauthentication", "no"),
            ("preferredauthentications", preferred), ("pubkeyauthentication", public_key),
            ("passwordauthentication", password), ("batchmode", batch),
        ] {
            let observed = values(option);
            let observed = match observed.as_slice() {
                ["true"] => &["yes"],
                ["false"] => &["no"],
                other => other,
            };
            assert_eq!(observed, [expected], "{option}");
        }
        assert!(values("globalknownhostsfile").iter().all(|value| *value == "none"));
        let known_hosts_path = plan.config.with_file_name("known_hosts");
        assert_eq!(values("userknownhostsfile"), [known_hosts_path.to_str().unwrap().replace('\\', "/")]);
        assert_eq!(fs::read_to_string(known_hosts_path).unwrap(), format!("[dev.example.invalid]:2222 {KEY}\n"));
        if batch == "yes" {
            let public_key_path = plan.config.with_file_name("agent.pub");
            assert_eq!(values("identityfile"), [public_key_path.to_str().unwrap().replace('\\', "/")]);
            assert_eq!(fs::read_to_string(public_key_path).unwrap(), format!("{KEY}\n"));
            assert!(values("identityagent").is_empty());
        } else {
            assert_eq!(values("identityagent"), ["none"]);
            let expected = if preferred == "password" { String::from("none") } else { key_path.to_str().unwrap().replace('\\', "/") };
            assert_eq!(values("identityfile"), [expected]);
        }
        let config = plan.config.clone();
        drop(plan);
        assert!(!config.exists());
    }
}

#[test]
fn f01_rejects_destination_and_config_syntax_in_input() {
    let (root, store, endpoint) = setup();
    for host in ["-oProxyCommand", "user@host", "*.invalid", "!host", "a,b", "host:22", "host\nPort 22", "host%h", "[::1]"] {
        assert!(matches!(Endpoint::new(host, 22), Err(Error::InvalidEndpoint)), "{host}");
    }
    assert!(matches!(Endpoint::new("test.invalid", 0), Err(Error::InvalidEndpoint)));
    assert_eq!(Endpoint::new("2001:0db8::1", 22).unwrap().alias(), "[2001:db8::1]:22");
    let approved = approve(&endpoint, SHA);
    store.register(&approved, KEY).unwrap();
    for account in ["-lroot", "root@elsewhere", "root\nHostName elsewhere", "root%u"] {
        assert!(matches!(
            ConnectionPlan::prepare(&store, &approved, account, Transport::Direct, Authentication::ManualPassword),
            Err(Error::InvalidAccount),
        ));
    }
    for name in ["key\"", "key\nInclude elsewhere", "%d", "${HOME}", "[key]", "../key"] {
        let path = root.path().join(name);
        assert!(matches!(
            ConnectionPlan::prepare(&store, &approved, "dev", Transport::Direct, Authentication::KeyFile { private_key: &path }),
            Err(Error::InvalidPath),
        ));
    }
    #[cfg(windows)]
    {
        for path in [r"C:relative", r"\\server\share\key", r"\\?\C:\key", r"C:\data\key:stream", r"C:\data\NUL.txt"] {
            assert!(matches!(config_path(Path::new(path)), Err(Error::InvalidPath)), "{path}");
        }
        assert_eq!(config_path(Path::new(r"C:\path with spaces\key")).unwrap(), "\"C:/path with spaces/key\"");
    }
}

#[test]
fn f01_redirected_trust_roots_and_ancestors_are_rejected() {
    let (root, _, _) = setup();
    let redirect = root.path().join("redirected folder");
    let parent = root.path().parent().unwrap();
    #[cfg(windows)]
    {
        // 정션은 관리자 권한과 개발자 모드 없이 임시 디렉터리에서 만들 수 있다.
        let script = "[void](New-Item -ItemType Junction -Path $env:F01_LINK -Target $env:F01_TARGET)";
        let output = Command::new(openssh_directory().unwrap().parent().unwrap()
            .join("WindowsPowerShell/v1.0/powershell.exe"))
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            .env("F01_LINK", &redirect).env("F01_TARGET", parent).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(parent, &redirect).unwrap();
    let direct = TrustStore::open(&redirect);
    let nested = TrustStore::open(&redirect.join(root.path().file_name().unwrap()));
    #[cfg(windows)]
    fs::remove_dir(&redirect).unwrap();
    #[cfg(unix)]
    fs::remove_file(&redirect).unwrap();
    assert!(matches!(direct, Err(Error::InvalidPath)));
    assert!(matches!(nested, Err(Error::InvalidPath)));
}
