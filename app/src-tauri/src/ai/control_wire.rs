//! Linux 제어 채널과 앱이 공유하는 JSON 프레임.

use serde::{Deserialize, Serialize};
use std::num::NonZeroU64;

pub(crate) const VERSION: u32 = 1;
pub(crate) const MAX_FRAME_BYTES: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WireOperation {
    Uptime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WireStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ClientFrame<'a> {
    Hello {
        version: u32,
        nonce: &'a str,
        channel: &'a str,
    },
    Start {
        channel: &'a str,
        job_id: NonZeroU64,
        seq: NonZeroU64,
        operation: WireOperation,
    },
    Stop {
        channel: &'a str,
        job_id: NonZeroU64,
        seq: NonZeroU64,
    },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum NodeFrame<'a> {
    Ready {
        version: u32,
        nonce: &'a str,
    },
    HelloAck {
        version: u32,
        channel: &'a str,
    },
    Started {
        channel: &'a str,
        job_id: NonZeroU64,
        seq: NonZeroU64,
    },
    Output {
        channel: &'a str,
        job_id: NonZeroU64,
        seq: NonZeroU64,
        stream: WireStream,
        data: &'a str,
    },
    Exited {
        channel: &'a str,
        job_id: NonZeroU64,
        seq: NonZeroU64,
        #[serde(deserialize_with = "nullable_status")]
        code: Option<i32>,
        #[serde(deserialize_with = "nullable_status")]
        signal: Option<i32>,
    },
}

fn nullable_status<'de, D: serde::Deserializer<'de>>(value: D) -> Result<Option<i32>, D::Error> {
    Option::deserialize(value)
}
