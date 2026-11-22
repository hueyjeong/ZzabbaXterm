use zeroize::Zeroizing;

#[cfg(target_os = "linux")]
pub(super) const SERVICE: &str = "dev.zzabbaxterm.desktop.vnc";

#[cfg(target_os = "linux")]
mod linux {
    use std::{collections::HashMap, time::Duration};
    use dbus::{Path, arg::Variant, blocking::{SyncConnection, stdintf::org_freedesktop_dbus::Properties}};
    use zeroize::Zeroizing;

    const SERVICE: &str = "org.freedesktop.Secret.Service";
    const ITEM: &str = "org.freedesktop.Secret.Item";
    const TIMEOUT: Duration = Duration::from_secs(2);
    type ItemPath = Path<'static>;
    type Secret = (ItemPath, Vec<u8>, Vec<u8>, String);

    #[derive(Debug, Eq, PartialEq)]
    struct Identity { path: ItemPath, created: u64, modified: u64, item_id: String }

    pub(super) struct Frozen {
        connection: SyncConnection,
        owner: String,
        identity: Identity,
        scope: String,
    }

    pub(super) fn freeze(scope: &str) -> Option<Frozen> {
        freeze_on(SyncConnection::new_session().ok()?, scope)
    }

    pub(super) fn freeze_on(connection: SyncConnection, scope: &str) -> Option<Frozen> {
        let bus = connection.with_proxy("org.freedesktop.DBus", "/org/freedesktop/DBus", TIMEOUT);
        let (owner,): (String,) = bus.method_call("org.freedesktop.DBus", "GetNameOwner", ("org.freedesktop.secrets",)).ok()?;
        if !owner.starts_with(':') { return None; }
        let identity = identity(&connection, &owner, scope)?;
        Some(Frozen { connection, owner, identity, scope: scope.into() })
    }

    fn identity(connection: &SyncConnection, owner: &str, scope: &str) -> Option<Identity> {
        let service = connection.with_proxy(owner, "/org/freedesktop/secrets", TIMEOUT);
        let attrs = HashMap::from([("service", super::SERVICE), ("target", "default"), ("user", scope)]);
        let (mut unlocked, locked): (Vec<ItemPath>, Vec<ItemPath>) = service.method_call(SERVICE, "SearchItems", (&attrs,)).ok()?;
        if !locked.is_empty() || unlocked.len() != 1 { return None; }
        let path = unlocked.pop()?;
        let item = connection.with_proxy(owner, path.clone(), TIMEOUT);
        let properties: HashMap<String, String> = item.get(ITEM, "Attributes").ok()?;
        if !attrs.iter().all(|(key, value)| properties.get(*key).is_some_and(|s| s == value)) { return None; }
        let item_id = properties.get("vnc-item-id")?.clone();
        if item_id.len() != 32 || !item_id.bytes().all(|b| b.is_ascii_hexdigit()) { return None; }
        if item.get::<bool>(ITEM, "Locked").ok()? { return None; }
        Some(Identity { path, created: item.get(ITEM, "Created").ok()?, modified: item.get(ITEM, "Modified").ok()?, item_id })
    }

    impl Frozen {
        pub(super) fn id(&self) -> &str { &self.identity.item_id }

        pub(super) fn read(&self, live: &impl Fn() -> bool) -> Option<Zeroizing<Vec<u8>>> {
            if !live() || identity(&self.connection, &self.owner, &self.scope).as_ref() != Some(&self.identity) { return None; }
            let service = self.connection.with_proxy(&self.owner, "/org/freedesktop/secrets", TIMEOUT);
            // 세션 버스의 고정된 고유 목적지에만 유니캐스트한다.
            let (_, session): (Variant<String>, ItemPath) = service.method_call(SERVICE, "OpenSession", ("plain", Variant(""))).ok()?;
            let result = (|| {
                if !live() || identity(&self.connection, &self.owner, &self.scope).as_ref() != Some(&self.identity) { return None; }
                let item = self.connection.with_proxy(&self.owner, self.identity.path.clone(), TIMEOUT);
                let ((returned, params, bytes, _),): (Secret,) = item.method_call(ITEM, "GetSecret", (session.clone(),)).ok()?;
                let bytes = Zeroizing::new(bytes);
                if returned != session || !params.is_empty() || !live() ||
                    identity(&self.connection, &self.owner, &self.scope).as_ref() != Some(&self.identity) { return None; }
                Some(bytes)
            })();
            let session = self.connection.with_proxy(&self.owner, session, TIMEOUT);
            let _: Result<(), _> = session.method_call("org.freedesktop.Secret.Session", "Close", ());
            result.filter(|_| live())
        }
    }
}

#[cfg(target_os = "linux")]
pub(super) struct Frozen(linux::Frozen);
#[cfg(not(target_os = "linux"))]
pub(super) struct Frozen(std::convert::Infallible);

impl Frozen {
    pub(super) fn freeze(scope: &str) -> Option<Self> {
        #[cfg(target_os = "linux")]
        { linux::freeze(scope).map(Self) }
        #[cfg(not(target_os = "linux"))]
        { let _ = scope; None }
    }

    pub(super) fn id(&self) -> &str {
        #[cfg(target_os = "linux")]
        { self.0.id() }
        #[cfg(not(target_os = "linux"))]
        { match self.0 {} }
    }

    pub(super) fn read(&self, live: impl Fn() -> bool) -> Option<Zeroizing<Vec<u8>>> {
        #[cfg(target_os = "linux")]
        { self.0.read(&live) }
        #[cfg(not(target_os = "linux"))]
        { let _ = live; match self.0 {} }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use dbus::{MessageType, Path, arg::Variant, blocking::{Connection, SyncConnection}, channel::{MatchingReceiver, Sender}, message::MatchRule};
    use std::{collections::HashMap, io::{BufRead, BufReader}, process::{Child, Command, Stdio}, sync::{Arc, atomic::{AtomicBool, AtomicUsize, Ordering}}, time::Duration};

    #[test]
    fn vnc_linux_vault_rejects_replacement_and_revocation_before_get_secret() {
        struct Bus(Child);
        impl Drop for Bus {
            fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); }
        }
        let mut bus = Bus(Command::new("dbus-daemon").args(["--session", "--nofork", "--print-address=1"])
            .stdout(Stdio::piped()).spawn().unwrap());
        let mut address = String::new();
        BufReader::new(bus.0.stdout.take().unwrap()).read_line(&mut address).unwrap();
        let address = address.trim().to_owned();
        let client = SyncConnection::new_address(&address).unwrap();
        let replacement = Arc::new(AtomicBool::new(false));
        let reads = Arc::new(AtomicUsize::new(0));
        let running = Arc::new(AtomicBool::new(true));
        let (ready, online) = std::sync::mpsc::channel();
        let server = {
            let (replacement, reads, running) = (replacement.clone(), reads.clone(), running.clone());
            std::thread::spawn(move || {
                let server = Connection::new_address(&address).unwrap();
                server.request_name("org.freedesktop.secrets", false, false, true).unwrap();
                let mut rule = MatchRule::new();
                rule.msg_type = Some(MessageType::MethodCall);
                server.start_receive(rule, Box::new(move |msg, conn| {
                    let path = msg.path().unwrap().to_string();
                    let member = msg.member().unwrap().to_string();
                    let reply = msg.method_return();
                    let path_for = |replacement| Path::new(if replacement { "/item/new" } else { "/item/approved" }).unwrap();
                    let reply = match member.as_str() {
                        "SearchItems" => {
                            assert_eq!(msg.read1::<HashMap<String, String>>().unwrap(), HashMap::from([
                                ("service".into(), SERVICE.into()), ("target".into(), "default".into()), ("user".into(), "[localhost]:5900".into()),
                            ]));
                            reply.append2(vec![path_for(replacement.load(Ordering::Acquire))], Vec::<Path>::new())
                        }
                        "Get" => match msg.read2::<String, String>().unwrap().1.as_str() {
                            "Attributes" => reply.append1(Variant(HashMap::from([
                                ("service", SERVICE), ("target", "default"), ("user", "[localhost]:5900"),
                                ("vnc-item-id", "1234567890abcdef1234567890abcdef"),
                            ]))),
                            "Locked" => reply.append1(Variant(false)),
                            "Created" | "Modified" => reply.append1(Variant(1_u64)),
                            _ => panic!("메타데이터: {path}"),
                        },
                        "OpenSession" => reply.append2(Variant(""), Path::new("/session/fixture").unwrap()),
                        "Close" => reply,
                        "GetSecret" => {
                            reads.fetch_add(1, Ordering::Relaxed);
                            reply.append1((Path::new("/session/fixture").unwrap(), Vec::<u8>::new(), b"isolated".to_vec(), "text/plain"))
                        }
                        _ => panic!("호출: {member}"),
                    };
                    conn.send(reply).unwrap();
                    true
                }));
                ready.send(()).unwrap();
                while running.load(Ordering::Acquire) { server.process(Duration::from_millis(20)).unwrap(); }
            })
        };
        online.recv().unwrap();
        let frozen = Frozen(linux::freeze_on(client, "[localhost]:5900").unwrap());
        replacement.store(true, Ordering::Release);
        assert!(frozen.read(|| true).is_none());
        replacement.store(false, Ordering::Release);
        assert!(frozen.read(|| false).is_none());
        assert_eq!(reads.load(Ordering::Relaxed), 0);
        assert_eq!(frozen.read(|| true).unwrap().as_slice(), b"isolated");
        assert_eq!(reads.load(Ordering::Relaxed), 1);
        running.store(false, Ordering::Release);
        server.join().unwrap();
    }
}
