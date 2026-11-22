//! A03 공유 제어 세션의 추가 프레임. 준비와 실행 이벤트는 A02 형식을 사용한다.

use serde::{Deserialize, Serialize};
use std::num::NonZeroU64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Ticket {
    pub(crate) main_id: NonZeroU64,
    pub(crate) main_generation: NonZeroU64,
    pub(crate) document: u64,
    pub(crate) control_id: NonZeroU64,
    pub(crate) control_generation: NonZeroU64,
    pub(crate) shell_generation: NonZeroU64,
    pub(crate) approval_id: NonZeroU64,
    pub(crate) input_generation: u64,
    pub(crate) policy_revision: NonZeroU64,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Client<'a> {
    Reserve {
        channel: &'a str,
        seq: NonZeroU64,
        ticket: Ticket,
        operation: super::control_wire::WireOperation,
    },
    Grant {
        channel: &'a str,
        seq: NonZeroU64,
        ticket: Ticket,
        operation: super::control_wire::WireOperation,
    },
    Deny {
        channel: &'a str,
        seq: NonZeroU64,
        ticket: Ticket,
    },
    Stop {
        channel: &'a str,
        seq: NonZeroU64,
        job_id: NonZeroU64,
    },
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Node<'a> {
    Pairing {
        channel: &'a str,
        seq: NonZeroU64,
        code: &'a str,
        expires_in_ms: u64,
    },
    Paired {
        channel: &'a str,
        seq: NonZeroU64,
        uid: u32,
        shell_generation: NonZeroU64,
    },
    Claim {
        channel: &'a str,
        seq: NonZeroU64,
        ticket: Ticket,
    },
}
