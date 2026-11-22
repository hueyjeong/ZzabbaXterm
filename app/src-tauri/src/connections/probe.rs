use std::{collections::HashMap, sync::Arc, thread};

use serde::{Deserialize, Serialize};

use crate::terminal::{Sessions, probe::Probe};
use super::{profile::{Draft, Method, ValidatedDraft}, store::{SaveStatus, Store, StoreError}};

const UNTESTED: &str = "이 초안의 현재 시험이 없습니다";

#[derive(Clone, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RunKey {
    draft_id: String,
    draft_version: u64,
    run_id: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Passed {
    #[serde(flatten)]
    run: RunKey,
    pub(super) session_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SaveRequest {
    run: RunKey,
    draft: Draft,
}

struct Run {
    key: RunKey,
    session_id: String,
    draft: Arc<ValidatedDraft>,
    probe: Arc<Probe>,
    confirmed: bool,
}

type Retired = (String, u64, RunKey, Arc<Probe>);

#[derive(Default)]
pub(crate) struct Runs {
    active: HashMap<String, HashMap<String, Run>>,
    retiring: Vec<Retired>,
    unconfirmed: HashMap<String, Retired>,
}

impl Runs {
    fn get(&self, owner: &str, key: &RunKey) -> Result<&Run, String> {
        self.active.get(owner).and_then(|runs| runs.get(&key.draft_id))
            .filter(|run| run.key == *key).ok_or_else(|| UNTESTED.into())
    }

    pub(crate) fn attached_probe(&self, owner: &str, id: &str) -> Option<&Probe> {
        self.active.get(owner)?.values().find(|run| run.session_id == id).map(|run| run.probe.as_ref())
    }

    fn retire(&mut self, owner: &str, document: u64, key: RunKey, probe: Arc<Probe>) {
        let entry = (owner.into(), document, key, probe);
        if !entry.3.finished() {
            entry.3.stop();
            self.retiring.push(entry);
        } else if entry.3.cleanup_unconfirmed() {
            self.unconfirmed.insert(entry.2.run_id.clone(), entry);
        }
    }

    pub(crate) fn reap(&mut self, run_id: &str, current_document: bool) {
        if let Some(index) = self.retiring.iter().position(|(_, _, key, _)| key.run_id == run_id) {
            let entry = self.retiring.swap_remove(index);
            if (current_document || entry.3.is_ssh()) && entry.3.cleanup_unconfirmed() {
                self.unconfirmed.insert(run_id.into(), entry);
            }
        }
    }

    pub(crate) fn stop_owner(&mut self, owner: &str, document: u64) {
        if let Some(runs) = self.active.remove(owner) {
            for run in runs.into_values() {
                self.retire(owner, document, run.key, run.probe);
            }
        }
        self.unconfirmed.retain(|_, (previous, _, _, probe)| previous != owner || probe.is_ssh());
    }

    pub(crate) fn closing_ssh(&self, owner: Option<&str>) -> Vec<Arc<Probe>> {
        self.active.iter().filter(|(label, _)| owner.is_none_or(|owner| owner == *label))
            .flat_map(|(_, runs)| runs.values().map(|run| &run.probe))
            .chain(self.retiring.iter().chain(self.unconfirmed.values())
                .filter(|(label, _, _, _)| owner.is_none_or(|owner| owner == label))
                .map(|(_, _, _, probe)| probe))
            .filter(|probe| probe.is_ssh()).cloned().collect()
    }

    pub(crate) fn stop_all(&mut self) -> Vec<Arc<Probe>> {
        let mut probes: Vec<_> = self.unconfirmed.drain().filter_map(|(_, (_, _, _, probe))| probe.is_ssh().then_some(probe)).collect();
        probes.extend(self.retiring.drain(..).map(|(_, _, _, probe)| probe));
        for run in self.active.drain().flat_map(|(_, runs)| runs.into_values()) {
            run.probe.stop();
            probes.push(run.probe);
        }
        probes
    }
}

pub(super) fn start(sessions: &Sessions, owner: &str, document: u64, draft_id: String, draft_version: u64, draft: Draft, config: std::path::PathBuf) -> Result<RunKey, String> {
    if draft_id.is_empty() || draft_version > (1 << 53) - 1 {
        return Err("초안 ID 또는 버전이 올바르지 않습니다".into());
    }
    sessions.with_probe_access(owner, document, matches!(draft.target.method, Method::Ssh { .. }), |runs| {
        let active = runs.active.entry(owner.into()).or_default();
        if active.get(&draft_id).is_some_and(|run| run.key.draft_version > draft_version) {
            return Err("이전 초안 버전입니다".into());
        }
        // 재시험이 입력 검증에 실패해도 이전 성공은 폐기한다.
        if let Some(previous) = active.remove(&draft_id) {
            runs.retire(owner, document, previous.key, previous.probe);
        }
        let draft = Arc::new(draft.validate().map_err(|error| format!("초안 검증: {error:?}"))?);
        let key = RunKey { draft_id, draft_version, run_id: sessions.next_id() };
        let session_id = sessions.next_id();
        let probe = Arc::new(Probe::for_draft(owner, &draft));
        let worker = Arc::clone(&probe);
        let tested = Arc::clone(&draft);
        let completed = sessions.probe_completion(owner.into(), document, key.run_id.clone());
        thread::Builder::new().name("connection-probe".into()).spawn(move || {
            worker.run(&tested, &config);
            completed();
        })
            .map_err(|error| format!("연결 시험: {error}"))?;
        let run = Run { key: key.clone(), session_id, draft, probe, confirmed: false };
        runs.active.get_mut(owner).unwrap().insert(key.draft_id.clone(), run);
        Ok(key)
    })
}

pub(super) fn attach(sessions: &Sessions, owner: &str, document: u64, key: &RunKey, rows: u16, cols: u16,
    output: tauri::ipc::Channel<tauri::ipc::InvokeResponseBody>) -> Result<String, String> {
    let (probe, id) = sessions.with_probes(owner, document, |runs| {
        let run = runs.get(owner, key)?;
        let size = crate::terminal::probe_size(rows, cols)?;
        run.probe.attach(size, output)?;
        Ok((Arc::clone(&run.probe), run.session_id.clone()))
    })?;
    probe.wait_open()?;
    sessions.with_probes(owner, document, |runs| { runs.get(owner, key)?; Ok(id) })
}

pub(super) fn wait(sessions: &Sessions, owner: &str, document: u64, key: &RunKey) -> Result<Passed, String> {
    let probe = sessions.with_probes(owner, document, |runs| Ok(Arc::clone(&runs.get(owner, key)?.probe)))?;
    let result = probe.wait();
    sessions.with_probes(owner, document, |runs| {
        if let Err(error) = result {
            remove_run(runs, owner, document, key)?;
            return Err(error);
        }
        runs.get(owner, key)?;
        let run = runs.active.get_mut(owner).unwrap().get_mut(&key.draft_id).unwrap();
        run.confirmed = true;
        Ok(Passed { run: key.clone(), session_id: run.session_id.clone() })
    })
}

pub(super) fn cancel(sessions: &Sessions, owner: &str, document: u64, key: &RunKey) -> Result<(), String> {
    let probe = sessions.with_probes(owner, document, |runs| {
        if let Ok(run) = runs.get(owner, key) {
            let probe = Arc::clone(&run.probe);
            remove_run(runs, owner, document, key)?;
            return Ok(Some((probe, false)));
        }
        if let Some((previous, generation, old, probe)) = runs.unconfirmed.get(&key.run_id) {
            if previous != owner || *generation != document || old != key { return Err(UNTESTED.into()); }
            return Ok(Some((Arc::clone(probe), true)));
        }
        if let Some((_, _, _, probe)) = runs.retiring.iter().find(|(previous, generation, old, _)| previous == owner && *generation == document && old == key) {
            return Ok(Some((Arc::clone(probe), probe.is_ssh())));
        }
        let belongs_to_other = runs.retiring.iter().any(|(previous, generation, old, probe)| {
            old.run_id == key.run_id && (previous != owner || probe.is_ssh() && *generation != document)
        }) || runs.active.iter().any(|(other, active)| {
            other != owner && active.values().any(|run| run.key.run_id == key.run_id)
        });
        if belongs_to_other {
            return Err(UNTESTED.into());
        }
        Ok(None)
    })?;
    if let Some((probe, retry)) = probe {
        probe.cancel_result(retry)?;
        if retry {
            sessions.with_probes(owner, document, |runs| {
                runs.unconfirmed.remove(&key.run_id);
                Ok(())
            })?;
        }
    }
    Ok(())
}

fn remove_run(runs: &mut Runs, owner: &str, document: u64, key: &RunKey) -> Result<(), String> {
    runs.get(owner, key)?;
    let run = runs.active.get_mut(owner).unwrap().remove(&key.draft_id).unwrap();
    runs.retire(owner, document, run.key, run.probe);
    Ok(())
}

pub(super) fn save_confirmed(sessions: &Sessions, owner: &str, document: u64, request: SaveRequest, store: &Store) -> Result<(), String> {
    let SaveRequest { run: key, draft } = request;
    let draft = draft.validate().map_err(|error| format!("초안 검증: {error:?}"))?;
    let expected = sessions.with_probes(owner, document, |runs| {
        let run = runs.get(owner, &key)?;
        if !run.confirmed || !run.draft.matches(&draft) {
            return Err(UNTESTED.into());
        }
        Ok(Arc::clone(&run.draft))
    })?;
    let authorize = || sessions.with_probes(owner, document, |runs| {
        let run = runs.get(owner, &key)?;
        if !run.confirmed || !Arc::ptr_eq(&run.draft, &expected) {
            return Err(UNTESTED.into());
        }
        Ok(())
    });
    let committed = || {
        let _ = sessions.with_probes(owner, document, |runs| {
            runs.get(owner, &key)?;
            runs.active.get_mut(owner).unwrap().remove(&key.draft_id);
            Ok(())
        });
    };
    match store.save_authorized(draft, authorize, committed).map_err(|error| match error {
        StoreError::Authorization(reason) => reason,
        other => format!("프로필 저장: {other:?}"),
    })? {
        SaveStatus::GroupConflict => Err("다른 작업이 그룹을 변경했습니다".into()),
        SaveStatus::TargetConflict => Err("다른 작업이 대상을 변경했습니다".into()),
        SaveStatus::Saved => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connections::tests::{TestDir, draft};
    use std::{sync::mpsc, time::Duration};

    #[test]
    fn f01_interactive_trial_requires_the_exact_live_run() {
        let dir = TestDir::new();
        let sessions = Sessions::default();
        let input = crate::connections::tests::ssh_draft(dir.path());
        let begin = || start(&sessions, "main", 0, "draft".into(), 1,
            serde_json::from_value(input.clone()).unwrap(), dir.path().into()).unwrap();
        let first = begin();
        let second = begin();
        let output = || tauri::ipc::Channel::new(|_| Ok(()));
        assert!(attach(&sessions, "main", 0, &first, 24, 80, output()).is_err());
        assert!(attach(&sessions, "other", 0, &second, 24, 80, output()).is_err());
        assert!(attach(&sessions, "main", 0, &second, 0, 80, output()).is_err());
        sessions.navigate("main");
        assert!(attach(&sessions, "main", 0, &second, 24, 80, output()).is_err());
        assert!(wait(&sessions, "main", 1, &second).is_err());
    }

    #[test]
    #[ignore = "루프백 SSH 검증에는 격리된 Python/Paramiko가 필요합니다"]
    fn f01_interactive_loopback_saves_and_reopens() {
        use std::{io::{BufRead, BufReader}, process::{Child, Command, Stdio}};
        use crate::ssh::{ApprovedHost, Endpoint, TrustStore, TRUST_DIRECTORY};
        use serde_json::json;
        use tauri::ipc::{Channel, InvokeResponseBody};

        // 이 검증은 테스트 전용 Python/Paramiko 경로를 명시한 환경에서 실행한다.
        let python = std::env::var_os("ZZABBA_F01_FIXTURE_PYTHON").expect("격리된 Python 실행 파일을 지정합니다");
        struct Server(Child);
        impl Drop for Server {
            fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); }
        }
        let dir = TestDir::new();
        let key_path = dir.path().join("encrypted-fixture-key");
        #[cfg(windows)]
        crate::ssh::terminal::restrict_fixture_key(&key_path);
        let mut server = Server(Command::new(python)
            .arg(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ssh/terminal/loopback_fixture.py"))
            .arg(&key_path).stdout(Stdio::piped()).spawn().unwrap());
        let mut line = String::new();
        BufReader::new(server.0.stdout.take().unwrap()).read_line(&mut line).unwrap();
        let (port, public) = line.trim_end().split_once(' ').expect("루프백 SSH 준비 출력");
        let port = port.parse::<u16>().unwrap();
        let fingerprint = ssh_key::PublicKey::from_openssh(public).unwrap().fingerprint(ssh_key::HashAlg::Sha256).to_string();
        let endpoint = Endpoint::new("127.0.0.1", port).unwrap();
        let trust = TrustStore::create(&dir.path().join(TRUST_DIRECTORY)).unwrap();
        trust.register(&ApprovedHost::confirm(&endpoint, &endpoint, &fingerprint).unwrap(), public).unwrap();
        let sessions = Sessions::default();
        let store = Store::new(dir.path());
        let mut input = crate::connections::tests::ssh_draft(dir.path());
        input["target"]["ssh"]["host"] = json!("127.0.0.1");
        input["target"]["ssh"]["port"] = json!(port);
        input["target"]["ssh"]["account"] = json!("fixture-user");
        input["target"]["ssh"]["hostKeyFingerprint"] = json!(fingerprint);
        let terminal = |secret: &[u8], id: &str, rx: mpsc::Receiver<InvokeResponseBody>, close_after: Option<&str>| {
            let mut output = Vec::new();
            let mut cursor_sent = false;
            let mut sent = false;
            let deadline = std::time::Instant::now() + Duration::from_secs(15);
            loop {
                match rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())) {
                    Ok(InvokeResponseBody::Raw(bytes)) => {
                        output.extend(bytes);
                        let _ = sessions.ack("main", id, output.len() as u64);
                        let text = String::from_utf8_lossy(&output);
                        if !cursor_sent && text.contains("\x1b[6n") {
                            sessions.input("main", id, b"\x1b[1;1R").unwrap();
                            cursor_sent = true;
                        }
                        if !sent && (text.contains("password:") || text.contains("passphrase for key")) {
                            sessions.input("main", id, secret).unwrap();
                            sent = true;
                        }
                        if close_after.is_some_and(|marker| text.contains(marker)) {
                            sessions.close_ssh_fixture(id);
                            break;
                        }
                    }
                    Ok(InvokeResponseBody::Json(event)) => {
                        assert!(close_after.is_none(), "저장된 셸이 닫기 요청 전에 종료됐습니다: {event}");
                        assert!(event.contains("exited") && event.contains("\"value\":0"), "{event}: {}", String::from_utf8_lossy(&output));
                        break;
                    }
                    Err(error) => panic!("{error}: {}", String::from_utf8_lossy(&output)),
                }
            }
            assert!(sent);
        };
        let cancelled = start(&sessions, "main", 0, "draft".into(), 0,
            serde_json::from_value(input.clone()).unwrap(), dir.path().into()).unwrap();
        let cancelled_id = attach(&sessions, "main", 0, &cancelled, 24, 100, Channel::new(|_| Ok(()))).unwrap();
        sessions.start("main", &cancelled_id).unwrap();
        cancel(&sessions, "main", 0, &cancelled).unwrap();
        assert!(sessions.input("main", &cancelled_id, b"blocked").is_err());
        assert!(wait(&sessions, "main", 0, &cancelled).is_err());
        let request = serde_json::from_value(json!({"run":cancelled,"draft":input})).unwrap();
        assert!(save_confirmed(&sessions, "main", 0, request, &store).is_err());

        for (index, auth) in [json!({"kind":"manualPassword"}), json!({"kind":"keyFile", "privateKeyPath": key_path})].into_iter().enumerate() {
            if index != 0 {
                input["groupRevision"] = json!(1);
                input["targetRevision"] = json!(1);
            }
            input["target"]["ssh"]["auth"] = auth;
            let run = start(&sessions, "main", 0, "draft".into(), index as u64,
                serde_json::from_value(input.clone()).unwrap(), dir.path().into()).unwrap();
            let (sender, rx) = mpsc::channel();
            let channel = Channel::new(move |body| { let _ = sender.send(body); Ok(()) });
            let id = attach(&sessions, "main", 0, &run, 24, 100, channel.clone()).unwrap();
            assert!(attach(&sessions, "main", 0, &run, 24, 100, channel).is_err());
            assert!(sessions.input("other", &id, b"blocked").is_err());
            let request = || serde_json::from_value(json!({"run":run,"draft":input})).unwrap();
            assert!(save_confirmed(&sessions, "main", 0, request(), &store).is_err());
            sessions.start("main", &id).unwrap();
            terminal(b"fixture-password\r", &id, rx, None);
            assert_eq!(wait(&sessions, "main", 0, &run).unwrap().session_id, id);
            save_confirmed(&sessions, "main", 0, request(), &store).unwrap();
            assert_eq!(serde_json::to_value(store.load().unwrap()).unwrap()["targets"][0]["profile"], input["target"]);
            if index == 0 {
                let (sender, rx) = mpsc::channel();
                let reopened = sessions.reopen_ssh_fixture(dir.path(), "ssh-server", Channel::new(move |body| {
                    let _ = sender.send(body); Ok(())
                })).unwrap();
                assert_ne!(reopened, id);
                sessions.start("main", &reopened).unwrap();
                terminal(b"fixture-password\r", &reopened, rx, Some("FIXTURE-REOPENED"));
            }
        }
        let closing = start(&sessions, "main", 0, "closing".into(), 0,
            serde_json::from_value(input.clone()).unwrap(), dir.path().into()).unwrap();
        let id = attach(&sessions, "main", 0, &closing, 24, 100, Channel::new(|_| Ok(()))).unwrap();
        sessions.start("main", &id).unwrap();
        assert!(sessions.close_ssh_probes(Some("other")));
        assert!(sessions.ack("main", &id, 0).is_ok());
        assert!(sessions.close_ssh_probes(Some("main")));
        assert!(sessions.input("main", &id, b"closed").is_err());
        sessions.destroy("main");
    }

    #[test]
    fn c01_cancel_retires_a_blocked_probe_until_worker_completion() {
        let dir = TestDir::new();
        let sessions = Sessions::default();
        let key = RunKey { draft_id: "draft".into(), draft_version: 1, run_id: sessions.next_id() };
        let draft: Draft = serde_json::from_value(draft(dir.path(), "work", "target")).unwrap();
        let draft = Arc::new(draft.validate().unwrap());
        let probe = Arc::new(Probe::new("main"));
        sessions.with_probes("main", 0, |runs| {
            runs.active.entry("main".into()).or_default().insert(key.draft_id.clone(), Run {
                key: key.clone(), session_id: sessions.next_id(), draft: Arc::clone(&draft),
                probe: Arc::clone(&probe), confirmed: false,
            });
            Ok(())
        }).unwrap();
        let completed = sessions.probe_completion("main".into(), 0, key.run_id.clone());
        let (tx, rx) = mpsc::channel();
        thread::scope(|scope| {
            let cancellation = || tx.send(cancel(&sessions, "main", 0, &key)).unwrap();
            scope.spawn(cancellation);
            let response = rx.recv_timeout(Duration::from_secs(2));
            let retired = response.is_ok() && !probe.finished() &&
                cancel(&sessions, "other", 0, &key).is_err() && wait(&sessions, "main", 0, &key).is_err();
            if retired {
                crate::terminal::tests::assert_live_shell_progress(&sessions);
            }
            probe.run(&draft, std::path::Path::new(""));
            completed();
            response.unwrap().unwrap();
            assert!(retired);
            assert!(cancel(&sessions, "other", 0, &key).is_ok());
        });
    }
}
