use std::{collections::HashMap, sync::Arc, thread};

use serde::{Deserialize, Serialize};

use crate::terminal::{Sessions, probe::Probe};
use super::{profile::{Draft, ValidatedDraft}, store::{SaveStatus, Store, StoreError}};

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

#[derive(Default)]
pub(crate) struct Runs {
    active: HashMap<String, HashMap<String, Run>>,
    retiring: Vec<(String, RunKey, Arc<Probe>)>,
    unconfirmed: HashMap<String, (String, RunKey, Arc<Probe>)>,
}

impl Runs {
    fn get(&self, owner: &str, key: &RunKey) -> Result<&Run, String> {
        self.active.get(owner).and_then(|runs| runs.get(&key.draft_id))
            .filter(|run| run.key == *key).ok_or_else(|| UNTESTED.into())
    }

    fn retire(&mut self, owner: &str, key: RunKey, probe: Arc<Probe>) {
        let entry = (owner.into(), key, probe);
        if !entry.2.finished() {
            entry.2.stop();
            self.retiring.push(entry);
        } else if entry.2.cleanup_unconfirmed() {
            self.unconfirmed.insert(entry.1.run_id.clone(), entry);
        }
    }

    pub(crate) fn reap(&mut self, run_id: &str, current_document: bool) {
        if let Some(index) = self.retiring.iter().position(|(_, key, _)| key.run_id == run_id) {
            let entry = self.retiring.swap_remove(index);
            if current_document && entry.2.cleanup_unconfirmed() { self.unconfirmed.insert(run_id.into(), entry); }
        }
    }

    pub(crate) fn stop_owner(&mut self, owner: &str) {
        if let Some(runs) = self.active.remove(owner) {
            for run in runs.into_values() {
                self.retire(owner, run.key, run.probe);
            }
        }
        self.unconfirmed.retain(|_, (previous, _, _)| previous != owner);
    }

    pub(crate) fn stop_all(&mut self) -> Vec<Arc<Probe>> {
        self.unconfirmed.clear();
        let mut probes: Vec<_> = self.retiring.drain(..).map(|(_, _, probe)| probe).collect();
        for run in self.active.drain().flat_map(|(_, runs)| runs.into_values()) {
            run.probe.stop();
            probes.push(run.probe);
        }
        probes
    }
}

pub(super) fn start(sessions: &Sessions, owner: &str, document: u64, draft_id: String, draft_version: u64, draft: Draft) -> Result<RunKey, String> {
    if draft_id.is_empty() || draft_version > (1 << 53) - 1 {
        return Err("초안 ID 또는 버전이 올바르지 않습니다".into());
    }
    sessions.with_probes(owner, document, |runs| {
        let active = runs.active.entry(owner.into()).or_default();
        if active.get(&draft_id).is_some_and(|run| run.key.draft_version > draft_version) {
            return Err("이전 초안 버전입니다".into());
        }
        // 재시험이 입력 검증에 실패해도 이전 성공은 폐기한다.
        if let Some(previous) = active.remove(&draft_id) {
            runs.retire(owner, previous.key, previous.probe);
        }
        let draft = Arc::new(draft.validate().map_err(|error| format!("초안 검증: {error:?}"))?);
        let key = RunKey { draft_id, draft_version, run_id: sessions.next_id() };
        let session_id = sessions.next_id();
        let probe = Arc::new(Probe::for_draft(owner, &draft));
        let worker = Arc::clone(&probe);
        let tested = Arc::clone(&draft);
        let completed = sessions.probe_completion(owner.into(), document, key.run_id.clone());
        thread::Builder::new().name("connection-probe".into()).spawn(move || {
            worker.run(&tested);
            completed();
        })
            .map_err(|error| format!("연결 시험: {error}"))?;
        let run = Run { key: key.clone(), session_id, draft, probe, confirmed: false };
        runs.active.get_mut(owner).unwrap().insert(key.draft_id.clone(), run);
        Ok(key)
    })
}

pub(super) fn wait(sessions: &Sessions, owner: &str, document: u64, key: &RunKey) -> Result<Passed, String> {
    let probe = sessions.with_probes(owner, document, |runs| Ok(Arc::clone(&runs.get(owner, key)?.probe)))?;
    let result = probe.wait();
    sessions.with_probes(owner, document, |runs| {
        if let Err(error) = result {
            remove_run(runs, owner, key)?;
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
            remove_run(runs, owner, key)?;
            return Ok(Some((probe, false)));
        }
        if let Some((previous, old, probe)) = runs.unconfirmed.get(&key.run_id) {
            if previous != owner || old != key { return Err(UNTESTED.into()); }
            return Ok(Some((Arc::clone(probe), true)));
        }
        if let Some((_, _, probe)) = runs.retiring.iter().find(|(previous, old, _)| previous == owner && old == key) {
            return Ok(Some((Arc::clone(probe), false)));
        }
        let belongs_to_other = runs.retiring.iter().any(|(previous_owner, old, _)| {
            previous_owner != owner && old.run_id == key.run_id
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

fn remove_run(runs: &mut Runs, owner: &str, key: &RunKey) -> Result<(), String> {
    runs.get(owner, key)?;
    let run = runs.active.get_mut(owner).unwrap().remove(&key.draft_id).unwrap();
    runs.retire(owner, run.key, run.probe);
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
        Ok((Arc::clone(&run.draft), Arc::clone(&run.probe)))
    })?;
    let tested_account_id = expected.1.wait()?;
    if matches!(draft.method(), super::profile::MethodRef::Ssm { .. }) != tested_account_id.is_some() {
        return Err(UNTESTED.into());
    }
    let authorize = || sessions.with_probes(owner, document, |runs| {
        let run = runs.get(owner, &key)?;
        if !run.confirmed || !Arc::ptr_eq(&run.draft, &expected.0) {
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
    match store.save_authorized(draft, tested_account_id, authorize, committed).map_err(|error| match error {
        StoreError::Authorization(reason) => reason,
        other => format!("프로필 저장: {other:?}"),
    })? {
        SaveStatus::GroupConflict => Err("다른 작업이 그룹을 변경했습니다".into()),
        SaveStatus::TargetConflict => Err("다른 작업이 대상을 변경했습니다".into()),
        SaveStatus::Saved => Ok(()),
    }
}

#[cfg(test)]
pub(crate) fn save_ssm_fixture(directory: &std::path::Path, input: serde_json::Value, account: &str) {
    let sessions = Sessions::default();
    let key = RunKey { draft_id: "fixture".into(), draft_version: 1, run_id: sessions.next_id() };
    let draft = Arc::new(serde_json::from_value::<Draft>(input.clone()).unwrap().validate().unwrap());
    let probe = Arc::new(Probe::for_draft("main", &draft));
    probe.finish_ssm_fixture(account);
    sessions.with_probes("main", 0, |runs| {
        runs.active.entry("main".into()).or_default().insert(key.draft_id.clone(), Run {
            key: key.clone(), session_id: sessions.next_id(), draft, probe, confirmed: false,
        });
        Ok(())
    }).unwrap();
    let request = || SaveRequest { run: key.clone(), draft: serde_json::from_value(input.clone()).unwrap() };
    let store = Store::new(directory);
    assert!(save_confirmed(&sessions, "main", 0, request(), &store).is_err());
    wait(&sessions, "main", 0, &key).unwrap();
    save_confirmed(&sessions, "main", 0, request(), &store).unwrap();
    assert!(save_confirmed(&sessions, "main", 0, request(), &store).is_err());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connections::tests::{TestDir, draft};
    use std::{sync::mpsc, time::Duration};

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
            probe.run(&draft);
            completed();
            response.unwrap().unwrap();
            assert!(retired);
            assert!(cancel(&sessions, "other", 0, &key).is_ok());
        });
    }
}
