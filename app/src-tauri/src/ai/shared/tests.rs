use super::*;
use std::{sync::{Barrier, Mutex}, thread, time::Duration};

const READY: Prompt = Prompt::EmptyForegroundReadline;

fn generation(value: u64) -> NonZeroU64 {
    NonZeroU64::new(value).unwrap()
}

fn binding() -> Binding {
    Binding {
        main: MainSession { id: 0, generation: generation(1), document: 0 },
        control: ControlSession { id: 1, generation: generation(1) },
        policy_revision: generation(1),
        blocked: false,
        mode: Mode::Review,
    }
}

fn paired(binding: Binding) -> SharedState {
    let mut state = SharedState::default();
    assert!(state.bind(binding, binding.main, 1000, 1000));
    state
}

fn open(state: &mut SharedState, now: Instant) -> Ticket {
    state.open_review(now + Duration::from_secs(1), now).unwrap()
}

fn approved(state: &mut SharedState, now: Instant) -> Ticket {
    let ticket = open(state, now);
    assert!(state.decide(ticket, UserDecision::Allow, now));
    ticket
}

#[test]
fn first_human_input_preserves_bytes_and_revokes_late_approval_or_claim() {
    let now = Instant::now();
    let binding = binding();
    let main = binding.main;
    let mut state = paired(binding);
    let old = open(&mut state, now);
    let mut pty = Vec::new();
    let mut grants = Vec::new();
    for bytes in [b"".as_slice(), "한글".as_bytes(), b"\x1b[200~paste\x1b[201~", b"\r"] {
        if let Input::Forward(bytes) = state.human_input(main, bytes, now) {
            pty.extend_from_slice(bytes);
        }
    }
    assert!(!state.decide(old, UserDecision::Allow, now));
    let current = approved(&mut state, now);
    assert!(current.input_generation > old.input_generation);
    assert_eq!(state.physical_key(main, READY, old, false, now), None);
    assert_eq!(state.physical_key(main, READY, current, false, now), Some(current));
    if let Input::Forward(bytes) = state.human_input(main, b"\x7f", now) {
        pty.extend_from_slice(bytes);
    }
    grants.extend(state.claim(binding.control, old, READY, now));
    grants.extend(state.claim(binding.control, current, READY, now));
    assert_eq!(pty, "한글\x1b[200~paste\x1b[201~\r\x7f".as_bytes());
    assert!(grants.is_empty());
}

#[test]
fn unverified_prompt_and_queued_input_yield_no_key_or_grant_in_any_mode() {
    let now = Instant::now();
    for mode in [Mode::Review, Mode::Auto, Mode::Bypass] {
        let binding = Binding { mode, ..binding() };
        let mut state = paired(binding);
        for (prompt, busy) in [(Prompt::Unverified, false), (READY, true)] {
            let ticket = approved(&mut state, now);
            assert_eq!(state.physical_key(binding.main, prompt, ticket, busy, now), None);
            assert_eq!(state.claim(binding.control, ticket, READY, now), None);
        }
        let ticket = approved(&mut state, now);
        assert_eq!(state.claim(binding.control, ticket, READY, now), None);
        assert_eq!(state.physical_key(binding.main, READY, ticket, false, now), Some(ticket));
        assert_eq!(state.claim(binding.control, ticket, Prompt::Unverified, now), None);
        assert_eq!(state.claim(binding.control, ticket, READY, now), None);
    }
}

#[test]
fn stale_ticket_is_inert_after_cancel_expiry_policy_document_or_disconnect() {
    let now = Instant::now();
    let binding = binding();
    for invalidate in 0..10 {
        let mut state = paired(binding);
        let ticket = approved(&mut state, now);
        assert_eq!(state.physical_key(binding.main, READY, ticket, false, now), Some(ticket));
        let mut claim_time = now;
        match invalidate {
            0 => state.cancel(ticket),
            1 => claim_time += Duration::from_secs(1),
            2 => state.release_target(binding.main),
            3 => state.disconnect_main(binding.main),
            4 => state.disconnect_control(binding.control),
            _ => {
                let mut new = binding;
                match invalidate {
                    5 => new.main.generation = generation(2),
                    6 => new.main.document += 1,
                    7 => new.policy_revision = generation(2),
                    8 => new.mode = Mode::Auto,
                    9 => new.blocked = true,
                    _ => unreachable!(),
                }
                assert!(state.bind(new, new.main, 1000, 1000));
            }
        }
        assert_eq!(state.claim(binding.control, ticket, READY, claim_time), None);
        assert!(!state.decide(ticket, UserDecision::Allow, claim_time));
        assert_eq!(state.human_input(binding.main, b"manual", claim_time), Input::Forward(b"manual"));
        assert_eq!(state.status(), None);
    }
}

#[test]
fn pairing_and_claim_use_the_verified_source_and_distinct_control_session() {
    let now = Instant::now();
    let binding = binding();
    let mut state = SharedState::default();
    let peer_tab = MainSession { id: 2, ..binding.main };
    assert!(!state.bind(binding, peer_tab, 1000, 1000));
    assert!(!state.bind(binding, binding.main, 1001, 1000));
    let same_id = Binding { control: ControlSession { id: binding.main.id, ..binding.control }, ..binding };
    assert!(!state.bind(same_id, binding.main, 1000, 1000));
    assert_eq!(state.open_review(now + Duration::from_secs(1), now), None);
    assert!(state.bind(binding, binding.main, 1000, 1000));
    let ticket = approved(&mut state, now);
    assert!(!state.decide(ticket, UserDecision::Allow, now));
    assert_eq!(state.physical_key(peer_tab, READY, ticket, false, now), None);
    assert_eq!(state.physical_key(binding.main, READY, ticket, false, now), Some(ticket));
    let wrong_channel = ControlSession { generation: generation(2), ..binding.control };
    assert_eq!(state.claim(wrong_channel, ticket, READY, now), None);
    assert_eq!(state.claim(binding.control, ticket, READY, now), Some(Operation::Uptime));
    state.handoff_failed(ticket);
    assert_eq!(state.human_input(binding.main, b"manual", now), Input::Forward(b"manual"));
    assert_eq!(state.claim(binding.control, ticket, READY, now), None);
    assert!(state.observe(binding.control, ticket, Event::Started, now).is_err());
}

#[test]
fn concurrent_duplicate_key_and_claim_grant_once_and_only_pause_the_original_tab() {
    let now = Instant::now();
    let binding = binding();
    let mut state = paired(binding);
    let ticket = approved(&mut state, now);
    let state = Mutex::new(state);
    for claim in [false, true] {
        let gate = Barrier::new(2);
        let contenders = || {
            gate.wait();
            let mut state = state.lock().unwrap();
            if claim {
                state.claim(binding.control, ticket, READY, now).is_some()
            } else {
                state.physical_key(binding.main, READY, ticket, false, now).is_some()
            }
        };
        let wins = thread::scope(|scope| {
            let first = scope.spawn(contenders);
            let second = contenders();
            usize::from(first.join().unwrap()) + usize::from(second)
        });
        assert_eq!(wins, 1);
    }
    let mut state = state.into_inner().unwrap();
    assert_eq!(state.human_input(binding.main, b"never replay", now), Input::Starting);
    let peer = MainSession { id: 4, ..binding.main };
    assert_eq!(state.human_input(peer, b"pass", now), Input::Forward(b"pass"));
    let expired = now + Duration::from_secs(1);
    assert_eq!(state.human_input(binding.main, b"pass", expired), Input::Forward(b"pass"));
    assert!(state.observe(binding.control, ticket, Event::Started, expired).is_err());
    assert_eq!(state.status(), Some((ticket, Status::TerminationUnconfirmed)));
}

#[test]
fn running_job_keeps_original_session_and_output_after_explicit_retarget() {
    let now = Instant::now();
    let binding = binding();
    let mut state = paired(binding);
    let ticket = approved(&mut state, now);
    assert_eq!(state.physical_key(binding.main, READY, ticket, false, now), Some(ticket));
    assert_eq!(state.claim(binding.control, ticket, READY, now), Some(Operation::Uptime));
    assert_eq!(state.observe(binding.control, ticket, Event::Started, now), Ok(None));
    assert_eq!(state.human_input(binding.main, b"pass", now), Input::Forward(b"pass"));
    state.release_target(binding.main);
    let other = Binding {
        main: MainSession { id: 2, ..binding.main },
        control: ControlSession { id: 3, ..binding.control },
        ..binding
    };
    assert!(state.bind(other, other.main, 1000, 1000));
    assert_eq!(state.status(), Some((ticket, Status::Running)));
    assert_eq!(state.open_review(now + Duration::from_secs(1), now), None);
    let output = || Event::Output(ChildOutput { stream: Stream::Stdout, bytes: b"child" });
    assert!(state.observe(other.control, ticket, output(), now).is_err());
    assert_eq!(state.observe(binding.control, ticket, output(), now).unwrap().unwrap().bytes, b"child");
    assert!(state.request_stop(ticket));
    assert_eq!(state.status(), Some((ticket, Status::StopRequested)));
    state.disconnect_control(other.control);
    assert_eq!(state.status(), Some((ticket, Status::StopRequested)));
    state.disconnect_control(binding.control);
    assert_eq!(state.status(), Some((ticket, Status::TerminationUnconfirmed)));
    assert!(state.observe(binding.control, ticket, Event::Exited { code: None }, now).is_err());
}

#[test]
fn approved_session_is_independent_of_the_displayed_session() {
    let now = Instant::now();
    let binding = binding();
    let mut state = paired(binding);
    let ticket = approved(&mut state, now);
    let displayed = MainSession { id: 2, ..binding.main };
    // 표시 탭을 바꿔도 상태 소유자는 대상 선택을 다시 받지 않는다.
    assert_eq!(state.human_input(displayed, b"manual", now), Input::Forward(b"manual"));
    assert_eq!(state.physical_key(displayed, READY, ticket, false, now), None);
    assert_eq!(state.physical_key(binding.main, READY, ticket, false, now), Some(ticket));
    assert_eq!(state.claim(binding.control, ticket, READY, now), Some(Operation::Uptime));
    assert_eq!(state.status(), Some((ticket, Status::AwaitingStart)));
}

#[test]
fn paste_revokes_pending_handoff_but_preserves_user_bytes() {
    let now = Instant::now();
    let binding = binding();
    let mut state = paired(binding);
    let ticket = approved(&mut state, now);
    assert_eq!(state.physical_key(binding.main, READY, ticket, false, now), Some(ticket));
    let paste = b"\x1b[200~paste\x1b[201~";
    assert_eq!(state.human_input(binding.main, paste, now), Input::Forward(paste));
    assert_eq!(state.claim(binding.control, ticket, READY, now), None);
    assert_eq!(state.status(), None);
}

#[test]
fn reconnect_never_reuses_an_approval_and_late_close_leaves_new_session_intact() {
    let now = Instant::now();
    let binding = binding();
    let mut state = paired(binding);
    let old = approved(&mut state, now);
    let reconnected = MainSession { generation: generation(2), ..binding.main };
    assert_eq!(state.physical_key(reconnected, READY, old, false, now), None);
    assert_eq!(state.physical_key(binding.main, READY, old, false, now), Some(old));
    assert!(state.bind(Binding { main: reconnected, ..binding }, reconnected, 1000, 1000));
    assert_eq!(state.claim(binding.control, old, READY, now), None);

    let fresh = approved(&mut state, now);
    state.disconnect_main(binding.main);
    assert_eq!(state.physical_key(reconnected, READY, fresh, false, now), Some(fresh));
    state.disconnect_main(reconnected);
    assert_eq!(state.claim(binding.control, fresh, READY, now), None);
    assert_eq!(state.status(), None);
}

#[test]
fn reconnect_marks_an_already_handed_off_run_unconfirmed() {
    let now = Instant::now();
    let binding = binding();
    let mut state = paired(binding);
    let ticket = approved(&mut state, now);
    assert_eq!(state.physical_key(binding.main, READY, ticket, false, now), Some(ticket));
    assert_eq!(state.claim(binding.control, ticket, READY, now), Some(Operation::Uptime));
    let main = MainSession { generation: generation(2), ..binding.main };
    assert!(state.bind(Binding { main, ..binding }, main, 1000, 1000));
    assert_eq!(state.status(), Some((ticket, Status::TerminationUnconfirmed)));
    assert!(state.observe(binding.control, ticket, Event::Started, now).is_err());
}
