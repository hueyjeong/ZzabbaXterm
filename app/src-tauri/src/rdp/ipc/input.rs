use std::collections::BTreeSet;

use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::rdp::{Channels, Error, Input, InputHandle, MouseButton, Pointer, SessionId};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Command {
    sequence: u64,
    epoch: u64,
    display_generation: u64,
    action: Action,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
enum Action {
    Active { active: bool },
    Key { code: u8, extended: bool, down: bool },
    Pointer { x: u16, y: u16, buttons: u8 },
    Release,
    Copy,
    Paste { text: String },
    Resize { width: u16, height: u16 },
}

#[derive(Default)]
pub(super) struct Controls {
    pub started: bool,
    sequence: u64,
    epoch: u64,
    active: bool,
    display_generation: u64,
    display: Option<(SessionId, InputHandle, (u16, u16))>,
    channels: Channels,
    keys: BTreeSet<(u8, bool)>,
    pointer: (u16, u16, u8),
    copy: Option<u64>,
}

const BUTTONS: [MouseButton; 3] = [MouseButton::Left, MouseButton::Right, MouseButton::Middle];

impl Controls {
    pub(super) fn display(&mut self, id: SessionId, handle: InputHandle, size: (u16, u16), generation: u64) {
        self.display_generation = generation;
        self.active = false;
        self.pointer.0 = self.pointer.0.min(size.0 - 1);
        self.pointer.1 = self.pointer.1.min(size.1 - 1);
        self.display = Some((id, handle, size));
    }

    pub(super) fn channels(&mut self, channels: Channels) { self.channels = channels; }
    pub(super) fn take_copy(&mut self) -> Option<u64> { self.copy.take().filter(|epoch| self.active && *epoch == self.epoch) }

    pub(super) async fn send(&mut self, life: &CancellationToken, command: Command) -> Result<(), Error> {
        if life.is_cancelled() || command.sequence != self.sequence + 1 || command.sequence > super::MAX_SAFE { return Err(Error::Closed); }
        let sequence = command.sequence;
        self.sequence = sequence;
        if command.display_generation == 0 || command.display_generation > self.display_generation { return Err(Error::Closed); }
        if command.display_generation < self.display_generation { return Err(Error::StaleDisplay); }
        let result = async {
            if let Action::Active { active } = command.action {
                if command.epoch <= self.epoch || command.epoch > super::MAX_SAFE { return Err(Error::Closed); }
                // 해제가 busy로 끝나면 같은 명령을 다시 받을 수 있게 상태는 해제 뒤에 바꾼다.
                self.release().await?;
                self.active = active;
                self.epoch = command.epoch;
                return Ok(());
            }
            if !self.active || self.epoch != command.epoch { return Err(Error::Closed); }
            let Some((_, _, size)) = self.display else { return Err(Error::Busy); };
            match command.action {
                Action::Active { .. } => unreachable!(),
                Action::Key { code, extended, down } => {
                    if !down && !self.keys.contains(&(code, extended)) { return Ok(()); }
                    self.write(Input::ScanCode { code, extended, released: !down }).await?;
                    if down { self.keys.insert((code, extended)); } else { self.keys.remove(&(code, extended)); }
                }
                Action::Pointer { x, y, buttons } => {
                    if x >= size.0 || y >= size.1 || buttons & !7 != 0 { return Err(Error::Bounds); }
                    let (_, _, previous) = self.pointer;
                    self.write(Input::Pointer { x, y, action: Pointer::Move }).await?;
                    self.pointer.0 = x;
                    self.pointer.1 = y;
                    for (index, button) in BUTTONS.into_iter().enumerate() {
                        let bit = 1 << index;
                        if (previous ^ buttons) & bit != 0 {
                            self.write(Input::Pointer { x, y, action: Pointer::Button { button, pressed: buttons & bit != 0 } }).await?;
                            self.pointer.2 ^= bit;
                        }
                    }
                }
                Action::Release => self.release().await?,
                Action::Copy => {
                    if self.copy.is_some() { return Err(Error::CopyPending); }
                    if !(self.channels.cliprdr_ready && self.channels.remote_clipboard_text) { return Err(Error::ChannelUnavailable); }
                    self.write(Input::ReadClipboardText).await?;
                    self.copy = Some(command.epoch);
                }
                Action::Paste { text } => {
                    if !self.channels.cliprdr_ready { return Err(Error::ChannelUnavailable); }
                    self.write(Input::OfferClipboardText(text)).await?;
                }
                Action::Resize { width, height } => self.write(Input::Resize { width, height }).await?,
            }
            Ok(())
        };
        let reply = tokio::select! { biased;
            () = life.cancelled() => Err(Error::Closed),
            reply = result => reply,
        };
        // busy는 적용되지 않은 입력이므로 같은 순번의 재전송을 받는다. 이미 보낸 일부 이벤트는 재전송해도 같은 상태가 된다.
        if reply == Err(Error::Busy) { self.sequence = sequence - 1; }
        reply
    }

    async fn write(&self, input: Input) -> Result<(), Error> {
        let (id, handle, _) = self.display.as_ref().ok_or(Error::Busy)?;
        handle.send(*id, Some(self.display_generation), input).await
    }

    async fn release(&mut self) -> Result<(), Error> {
        while let Some((code, extended)) = self.keys.first().copied() {
            self.write(Input::ScanCode { code, extended, released: true }).await?;
            self.keys.remove(&(code, extended));
        }
        let (x, y, buttons) = self.pointer;
        for (index, button) in BUTTONS.into_iter().enumerate() {
            let bit = 1 << index;
            if buttons & bit != 0 {
                self.write(Input::Pointer { x, y, action: Pointer::Button { button, pressed: false } }).await?;
                self.pointer.2 &= !bit;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn old_display_is_discarded_but_current_coordinates_are_validated() {
        let mut controls = Controls::default();
        let id = SessionId(1);
        let (input, mut commands) = tokio::sync::mpsc::channel(1);
        controls.display(id, InputHandle { id, input }, (640, 480), 2);
        let life = CancellationToken::new();
        let command = |sequence, display_generation, action| Command { sequence, display_generation, epoch: 1, action };
        let pointer = || Action::Pointer { x: 640, y: 0, buttons: 1 };
        assert_eq!(controls.send(&life, command(1, 1, pointer())).await, Err(Error::StaleDisplay));
        assert_eq!(controls.send(&life, command(2, 2, Action::Active { active: true })).await, Ok(()));
        assert_eq!(controls.send(&life, command(3, 2, pointer())).await, Err(Error::Bounds));
        assert!(commands.try_recv().is_err());
    }

    /// 세션 작업 대신 정해 둔 결과로 쓰기 명령에 답하고, 받은 명령을 문자열로 남긴다.
    fn scripted(replies: Vec<Result<(), Error>>) -> (Controls, tokio::task::JoinHandle<Vec<String>>) {
        let mut controls = Controls::default();
        let id = SessionId(1);
        let (input, mut commands) = tokio::sync::mpsc::channel(1);
        controls.display(id, InputHandle { id, input }, (640, 480), 1);
        let task = tokio::spawn(async move {
            let mut replies = replies.into_iter();
            let mut log = Vec::new();
            while let Some((_, input, ack)) = commands.recv().await {
                log.push(match input {
                    Input::ScanCode { code, released, .. } => format!("key {code} {}", if released { "up" } else { "down" }),
                    Input::Pointer { x, y, action: Pointer::Move } => format!("move {x},{y}"),
                    Input::Pointer { action: Pointer::Button { pressed, .. }, .. } => format!("button {}", if pressed { "down" } else { "up" }),
                    _ => "other".into(),
                });
                let _ = ack.send(replies.next().unwrap_or(Ok(())));
            }
            log
        });
        (controls, task)
    }

    fn command(sequence: u64, epoch: u64, action: Action) -> Command {
        Command { sequence, display_generation: 1, epoch, action }
    }

    #[tokio::test]
    async fn pointer_busy_after_move_is_resent_with_the_same_sequence() {
        let (mut controls, log) = scripted(vec![Ok(()), Err(Error::Busy)]);
        let life = CancellationToken::new();
        let press = || Action::Pointer { x: 10, y: 20, buttons: 1 };
        assert_eq!(controls.send(&life, command(1, 1, Action::Active { active: true })).await, Ok(()));
        assert_eq!(controls.send(&life, command(2, 1, press())).await, Err(Error::Busy));
        assert_eq!(controls.pointer, (10, 20, 0));
        assert_eq!(controls.send(&life, command(2, 1, press())).await, Ok(()));
        assert_eq!(controls.pointer, (10, 20, 1));
        assert_eq!(controls.send(&life, command(3, 1, press())).await, Ok(()));
        drop(controls);
        assert_eq!(log.await.unwrap(), ["move 10,20", "button down", "move 10,20", "button down", "move 10,20"]);
    }

    #[tokio::test]
    async fn active_busy_during_release_keeps_the_old_epoch_until_resent() {
        let (mut controls, log) = scripted(vec![Ok(()), Err(Error::Busy)]);
        let life = CancellationToken::new();
        let key = |down| Action::Key { code: 0x1e, extended: false, down };
        assert_eq!(controls.send(&life, command(1, 1, Action::Active { active: true })).await, Ok(()));
        assert_eq!(controls.send(&life, command(2, 1, key(true))).await, Ok(()));
        assert_eq!(controls.send(&life, command(3, 2, Action::Active { active: false })).await, Err(Error::Busy));
        assert_eq!((controls.epoch, controls.active, controls.keys.len()), (1, true, 1));
        assert_eq!(controls.send(&life, command(3, 2, Action::Active { active: false })).await, Ok(()));
        assert_eq!((controls.epoch, controls.active, controls.keys.len()), (2, false, 0));
        assert_eq!(controls.send(&life, command(4, 2, key(true))).await, Err(Error::Closed));
        drop(controls);
        assert_eq!(log.await.unwrap(), ["key 30 down", "key 30 up", "key 30 up"]);
    }

    #[tokio::test]
    async fn pending_copy_is_reported_without_busy() {
        let (mut controls, _log) = scripted(Vec::new());
        controls.channels(Channels { cliprdr_ready: true, remote_clipboard_text: true, ..Default::default() });
        let life = CancellationToken::new();
        assert_eq!(controls.send(&life, command(1, 1, Action::Active { active: true })).await, Ok(()));
        assert_eq!(controls.send(&life, command(2, 1, Action::Copy)).await, Ok(()));
        assert_eq!(controls.send(&life, command(3, 1, Action::Copy)).await, Err(Error::CopyPending));
        assert_eq!(controls.send(&life, command(4, 1, Action::Key { code: 0x1e, extended: false, down: true })).await, Ok(()));
    }
}
