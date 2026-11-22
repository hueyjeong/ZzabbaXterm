//! 명시적으로 선택한 메인 세션의 승인과 입력을 관리하는 순수 상태.

use std::{num::NonZeroU64, time::Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Review,
    Auto,
    Bypass,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UserDecision {
    Allow,
    Reject,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Status {
    AwaitingStart,
    Running,
    StopRequested,
    Exited { code: Option<i32> },
    TerminationUnconfirmed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stream {
    Stdout,
    Stderr,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ChildOutput<'a> {
    pub(crate) stream: Stream,
    pub(crate) bytes: &'a [u8],
}

pub(crate) enum Event<'a> {
    Started,
    Output(ChildOutput<'a>),
    Exited { code: Option<i32> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Operation {
    Uptime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MainSession {
    pub(crate) id: u64,
    pub(crate) generation: NonZeroU64,
    pub(crate) document: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ControlSession {
    pub(crate) id: u64,
    pub(crate) generation: NonZeroU64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Binding {
    pub(crate) main: MainSession,
    pub(crate) control: ControlSession,
    pub(crate) policy_revision: NonZeroU64,
    pub(crate) blocked: bool,
    pub(crate) mode: Mode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Prompt {
    EmptyForegroundReadline,
    Unverified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Ticket {
    pub(crate) binding: Binding,
    pub(crate) approval_id: u64,
    pub(crate) input_generation: u64,
}

impl Ticket {
    pub(crate) fn operation(self) -> Operation {
        Operation::Uptime
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Input<'a> {
    Forward(&'a [u8]),
    Starting,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReviewPhase {
    Pending,
    Approved,
    PhysicalKey,
}

struct Review {
    ticket: Ticket,
    deadline: Instant,
    phase: ReviewPhase,
}

struct Run {
    ticket: Ticket,
    start_deadline: Instant,
    status: Status,
}

#[derive(Default)]
pub(crate) struct SharedState {
    binding: Option<Binding>,
    last_approval: u64,
    input_generation: u64,
    review: Option<Review>,
    run: Option<Run>,
}

impl SharedState {
    // 짝 연결 코드를 소비하고 선택 창·문서와 실제 입력 출처를 확인한 어댑터만 호출한다.
    pub(crate) fn bind(&mut self, binding: Binding, source: MainSession, peer_uid: u32, control_uid: u32) -> bool {
        if binding.main != source || binding.main.id == binding.control.id || peer_uid != control_uid {
            return false;
        }
        if self.binding != Some(binding) {
            self.lose_run(|running| {
                running.main.id == binding.main.id
                    && running.main.generation != binding.main.generation
            });
            if let Some(previous) = self.binding
                && previous.main == binding.main && previous.control != binding.control
            {
                self.disconnect_control(previous.control);
            }
            self.review = None;
            self.binding = Some(binding);
        }
        true
    }

    pub(crate) fn open_review(&mut self, deadline: Instant, now: Instant) -> Option<Ticket> {
        self.expire(now);
        let binding = self.binding.filter(|binding| !binding.blocked)?;
        if deadline <= now || self.run.as_ref().is_some_and(|run| in_flight(run.status)) {
            return None;
        }
        self.last_approval = self.last_approval.checked_add(1)?;
        let ticket = Ticket { binding, approval_id: self.last_approval, input_generation: self.input_generation };
        self.review = Some(Review { ticket, deadline, phase: ReviewPhase::Pending });
        Some(ticket)
    }

    pub(crate) fn decide(&mut self, ticket: Ticket, decision: UserDecision, now: Instant) -> bool {
        self.expire(now);
        let Some(review) = &mut self.review else { return false };
        if review.ticket != ticket || review.phase != ReviewPhase::Pending {
            return false;
        }
        match decision {
            UserDecision::Allow => review.phase = ReviewPhase::Approved,
            UserDecision::Reject | UserDecision::Cancel => self.review = None,
        }
        true
    }

    pub(crate) fn cancel(&mut self, ticket: Ticket) {
        if self.review.as_ref().is_some_and(|review| review.ticket == ticket) {
            self.review = None;
        }
    }

    // 조합 시작·키·붙여넣기 시작은 대기열에 들어가기 전에 같은 세션 순서로 처리한다.
    // 조합 시작처럼 바이트가 아직 없는 이벤트도 빈 슬라이스로 이 경계를 통과한다.
    pub(crate) fn human_input<'a>(&mut self, main: MainSession, bytes: &'a [u8], now: Instant) -> Input<'a> {
        self.expire(now);
        if self.run.as_ref().is_some_and(|run| run.ticket.binding.main == main && run.status == Status::AwaitingStart) {
            return Input::Starting;
        }
        if self.binding.is_some_and(|binding| binding.main == main) {
            self.review = None;
            // 승인 ID도 증가하므로 포화 이후에도 예전 티켓을 재사용할 수 없다.
            self.input_generation = self.input_generation.saturating_add(1);
        }
        Input::Forward(bytes)
    }

    // 반환된 티켓은 이미 수신한 물리 키 하나만 통과시킨다. 키 바이트를 만들지 않는다.
    pub(crate) fn physical_key(
        &mut self,
        main: MainSession,
        prompt: Prompt,
        ticket: Ticket,
        composing_or_queued: bool,
        now: Instant,
    ) -> Option<Ticket> {
        self.expire(now);
        let review = self.review.as_mut()?;
        if review.ticket != ticket || ticket.binding.main != main || review.phase != ReviewPhase::Approved {
            return None;
        }
        if prompt != Prompt::EmptyForegroundReadline || composing_or_queued {
            self.review = None;
            return None;
        }
        review.phase = ReviewPhase::PhysicalKey;
        Some(review.ticket)
    }

    // 인증된 채널의 처리기가 그 자리에서 포그라운드와 빈 읽기 줄을 다시 확인한다.
    // 호출자는 같은 상태 소유자에서 승인 응답을 한 번만 인계한다.
    pub(crate) fn claim(&mut self, control: ControlSession, ticket: Ticket, prompt: Prompt, now: Instant) -> Option<Operation> {
        self.expire(now);
        let review = self.review.as_ref()?;
        if control != ticket.binding.control {
            return None;
        }
        if review.ticket != ticket || review.phase != ReviewPhase::PhysicalKey
            || self.binding != Some(ticket.binding) || self.input_generation != ticket.input_generation
        {
            return None;
        }
        if prompt != Prompt::EmptyForegroundReadline {
            self.review = None;
            return None;
        }
        let start_deadline = self.review.take()?.deadline;
        self.run = Some(Run { ticket, start_deadline, status: Status::AwaitingStart });
        Some(ticket.operation())
    }

    // 표시 탭 변경과 별개인 명시적 대상 해제다. 이미 인계한 작업은 기존 바인딩을 유지한다.
    pub(crate) fn release_target(&mut self, main: MainSession) {
        if self.binding.is_some_and(|binding| binding.main == main) {
            self.binding = None;
            self.review = None;
        }
    }

    pub(crate) fn disconnect_main(&mut self, main: MainSession) {
        self.release_target(main);
        self.lose_run(|binding| binding.main == main);
    }

    pub(crate) fn disconnect_control(&mut self, control: ControlSession) {
        if self.binding.is_some_and(|binding| binding.control == control) {
            self.binding = None;
            self.review = None;
        }
        self.lose_run(|binding| binding.control == control);
    }

    fn lose_run(&mut self, matches: impl FnOnce(Binding) -> bool) {
        if let Some(run) = &mut self.run
            && in_flight(run.status) && matches(run.ticket.binding)
        {
            run.status = Status::TerminationUnconfirmed;
        }
    }

    pub(crate) fn handoff_failed(&mut self, ticket: Ticket) {
        if let Some(run) = &mut self.run
            && run.ticket == ticket && run.status == Status::AwaitingStart
        {
            run.status = Status::TerminationUnconfirmed;
        }
    }

    pub(crate) fn expire(&mut self, now: Instant) {
        if self.review.as_ref().is_some_and(|review| now >= review.deadline) {
            self.review = None;
        }
        if let Some(run) = &mut self.run
            && run.status == Status::AwaitingStart && now >= run.start_deadline
        {
            run.status = Status::TerminationUnconfirmed;
        }
    }

    pub(crate) fn status(&self) -> Option<(Ticket, Status)> {
        self.run.as_ref().map(|run| (run.ticket, run.status))
    }

    pub(crate) fn request_stop(&mut self, ticket: Ticket) -> bool {
        let Some(run) = self.run.as_mut().filter(|run| run.ticket == ticket) else { return false };
        if run.status != Status::Running {
            return false;
        }
        run.status = Status::StopRequested;
        true
    }

    pub(crate) fn observe<'a>(
        &mut self, control: ControlSession, ticket: Ticket, event: Event<'a>, now: Instant,
    ) -> Result<Option<ChildOutput<'a>>, ()> {
        self.expire(now);
        if control != ticket.binding.control {
            return Err(());
        }
        let run = self.run.as_mut().filter(|run| run.ticket == ticket).ok_or(())?;
        match (event, run.status) {
            (Event::Started, Status::AwaitingStart) => run.status = Status::Running,
            (Event::Output(output), Status::Running | Status::StopRequested) => return Ok(Some(output)),
            (Event::Exited { code }, Status::Running | Status::StopRequested) => run.status = Status::Exited { code },
            _ => return Err(()),
        }
        Ok(None)
    }
}

fn in_flight(status: Status) -> bool {
    matches!(status, Status::AwaitingStart | Status::Running | Status::StopRequested)
}

#[cfg(test)]
#[path = "shared/tests.rs"]
mod tests;
