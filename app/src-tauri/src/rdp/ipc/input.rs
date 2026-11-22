use std::collections::BTreeSet;

use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::rdp::{Channels, Error, Input, InputHandle, MouseButton, Pointer, SessionId};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Command {
    sequence: u64,
    epoch: u64,
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
    display: Option<(SessionId, InputHandle, (u16, u16))>,
    channels: Channels,
    keys: BTreeSet<(u8, bool)>,
    pointer: (u16, u16, u8),
    copy: Option<u64>,
}

const BUTTONS: [MouseButton; 3] = [MouseButton::Left, MouseButton::Right, MouseButton::Middle];

impl Controls {
    pub(super) fn display(&mut self, id: SessionId, handle: InputHandle, size: (u16, u16)) {
        self.pointer.0 = self.pointer.0.min(size.0 - 1);
        self.pointer.1 = self.pointer.1.min(size.1 - 1);
        self.display = Some((id, handle, size));
    }

    pub(super) fn channels(&mut self, channels: Channels) { self.channels = channels; }
    pub(super) fn take_copy(&mut self) -> Option<u64> { self.copy.take().filter(|epoch| self.active && *epoch == self.epoch) }

    pub(super) async fn send(&mut self, life: &CancellationToken, command: Command) -> Result<(), Error> {
        if life.is_cancelled() || command.sequence != self.sequence + 1 || command.sequence > super::MAX_SAFE { return Err(Error::Closed); }
        self.sequence = command.sequence;
        let result = async {
            if let Action::Active { active } = command.action {
                if command.epoch <= self.epoch || command.epoch > super::MAX_SAFE { return Err(Error::Closed); }
                self.active = active;
                self.epoch = command.epoch;
                return self.release().await;
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
                    for (index, button) in BUTTONS.into_iter().enumerate() {
                        let bit = 1 << index;
                        if (previous ^ buttons) & bit != 0 {
                            self.write(Input::Pointer { x, y, action: Pointer::Button { button, pressed: buttons & bit != 0 } }).await?;
                        }
                    }
                    self.pointer = (x, y, buttons);
                }
                Action::Release => self.release().await?,
                Action::Copy => {
                    if self.copy.is_some() { return Err(Error::Busy); }
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
        tokio::select! { biased;
            () = life.cancelled() => Err(Error::Closed),
            reply = result => reply,
        }
    }

    async fn write(&self, input: Input) -> Result<(), Error> {
        let (id, handle, _) = self.display.as_ref().ok_or(Error::Busy)?;
        handle.input(*id, input).await
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
