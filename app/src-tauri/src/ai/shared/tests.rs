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
