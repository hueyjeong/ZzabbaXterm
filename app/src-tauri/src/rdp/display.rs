use ironrdp::{
    core::{Decode as _, ReadCursor, impl_as_any},
    displaycontrol::{
        CHANNEL_NAME,
        pdu::{DisplayControlMonitorLayout, DisplayControlPdu},
    },
    dvc::{DvcClientProcessor, DvcMessage, DvcProcessor, encode_dvc_messages},
    pdu::{PduResult, decode_err},
    session::ActiveStage,
    svc::ChannelFlags,
};

use super::Error;

/// Display Control(MS-RDPEDISP) 동적 채널이다.
///
/// IronRDP 0.8의 `DisplayControlClient`는 능력 PDU 앞의 머리글(종류·길이 8바이트)을 건너뛰지 않고 읽는다.
/// 그래서 xrdp가 알린 모니터 16개·4096×2048이 5×20×16=1600화소로 계산되어 모든 해상도 변경이 범위 오류로 막혔다.
/// 이 처리기는 머리글부터 읽어 서버가 알린 최대 면적을 그대로 쓴다.
#[derive(Default)]
pub(super) struct DisplayControl {
    max_area: Option<u64>,
}

impl DisplayControl {
    /// 서버 능력 PDU를 받은 뒤에만 값이 있다.
    pub(super) fn max_area(&self) -> Option<u64> {
        self.max_area
    }
}

impl_as_any!(DisplayControl);

impl DvcProcessor for DisplayControl {
    fn channel_name(&self) -> &str {
        CHANNEL_NAME
    }

    fn start(&mut self, _channel_id: u32) -> PduResult<Vec<DvcMessage>> {
        Ok(Vec::new())
    }

    fn process(&mut self, _channel_id: u32, payload: &[u8]) -> PduResult<Vec<DvcMessage>> {
        let pdu = DisplayControlPdu::decode(&mut ReadCursor::new(payload)).map_err(|error| decode_err!(error))?;
        if let DisplayControlPdu::Caps(caps) = pdu {
            self.max_area = Some(caps.max_monitor_area());
        }
        Ok(Vec::new())
    }
}

impl DvcClientProcessor for DisplayControl {}

/// 크기 범위: MS-RDPEDISP 2.2.2.2.1(200 이상, 짝수 폭), 앱 프레임 한도, 서버가 알린 최대 면적.
pub(super) fn check(width: u16, height: u16, max_area: u64) -> Result<(), Error> {
    super::frame::frame_len(width, height)?;
    if width < 200 || height < 200 || width % 2 != 0 {
        return Err(Error::Bounds);
    }
    if u64::from(width) * u64::from(height) > max_area {
        return Err(Error::DisplayLimit);
    }
    Ok(())
}

/// 데스크톱 배율(%)은 100~500만 서버에 보낸다. 그 밖의 값은 보내지 않는다.
pub(super) fn scale(percent: u32) -> Option<u32> {
    (100..=500).contains(&percent).then_some(percent)
}

pub(super) fn encode_resize(stage: &mut ActiveStage, width: u16, height: u16, percent: u32) -> Result<Vec<u8>, Error> {
    let channel = stage
        .get_dvc::<DisplayControl>()
        .and_then(|channel| channel.channel_id())
        .ok_or(Error::ChannelUnavailable)?;
    let layout = DisplayControlMonitorLayout::new_single_primary_monitor(width.into(), height.into(), scale(percent), None)
        .map_err(|_| Error::Bounds)?;
    let messages = encode_dvc_messages(channel, vec![Box::new(DisplayControlPdu::from(layout))], ChannelFlags::empty())
        .map_err(|_| Error::Protocol)?;
    stage.encode_dvc_messages(messages).map_err(|_| Error::Protocol)
}

/// 보낸 해상도 변경이 재활성화로 끝나기 전까지의 상태다.
/// xrdp 0.9.24는 Deactivate All보다 먼저 새 크기 좌표의 비트맵을 보낸다. 그 갱신만 그리지 않고 버린다.
#[derive(Default)]
pub(super) struct Resizing(Option<(std::time::Instant, u16, u16)>);

impl Resizing {
    /// 서버가 재활성화하지 않아도 이 시간이 지나면 다시 모든 범위 오류로 세션을 끝낸다.
    const LIMIT: std::time::Duration = super::IO_TIMEOUT;

    pub(super) fn requested(&mut self, width: u16, height: u16) {
        self.0 = Some((std::time::Instant::now() + Self::LIMIT, width, height));
    }

    pub(super) fn reactivated(&mut self) {
        self.0 = None;
    }

    /// 현재 화면 밖이지만 요청한 크기 안을 가리키는 화면 갱신이면 `None`이다. 형식·길이·채널 오류는 그대로 돌려준다.
    pub(super) fn skip<T>(&self, result: Result<T, Error>) -> Result<Option<T>, Error> {
        match (result, self.0) {
            (Ok(value), _) => Ok(Some(value)),
            (Err(Error::OutOfImage { right, bottom }), Some((until, width, height)))
                if right < width && bottom < height && std::time::Instant::now() < until => Ok(None),
            (Err(error), _) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironrdp::displaycontrol::client::DisplayControlClient;
    use std::sync::{Arc, atomic::{AtomicU64, Ordering}};

    /// xrdp 0.9.24 `dynamic_monitor_open_response`가 보내는 능력 PDU다: 종류 5, 길이 20, 모니터 16, 4096, 2048.
    fn xrdp_caps() -> Vec<u8> {
        [5_u32, 20, 16, 4096, 2048].into_iter().flat_map(u32::to_le_bytes).collect()
    }

    #[test]
    fn ironrdp_client_misreads_xrdp_caps_and_rejects_1280x720() {
        // R6b 재현: 예전 경로는 IronRDP가 계산한 면적으로 크기를 검사했다.
        let area = Arc::new(AtomicU64::new(0));
        let mut client = DisplayControlClient::new({
            let area = Arc::clone(&area);
            move |caps| {
                area.store(caps.max_monitor_area(), Ordering::Relaxed);
                Ok(Vec::new())
            }
        });
        client.process(1, &xrdp_caps()).unwrap();
        assert_eq!(area.load(Ordering::Relaxed), 5 * 20 * 16);
        assert_eq!(check(1280, 720, area.load(Ordering::Relaxed)), Err(Error::DisplayLimit));
    }

    #[test]
    fn display_control_reads_the_header_and_accepts_1280x720_from_xrdp() {
        let mut channel = DisplayControl::default();
        assert_eq!(channel.max_area(), None);
        assert!(channel.process(1, &xrdp_caps()).unwrap().is_empty());
        let area = channel.max_area().unwrap();
        assert_eq!(area, 16 * 4096 * 2048);
        assert_eq!(check(1280, 720, area), Ok(()));
        assert_eq!(check(3840, 2160, area), Ok(()));
    }

    #[test]
    fn resize_bounds_follow_the_spec_the_frame_limit_and_the_server_area() {
        let area = 1920 * 1080;
        assert_eq!(check(1920, 1080, area), Ok(()));
        assert_eq!(check(1922, 1080, area), Err(Error::DisplayLimit));
        assert_eq!(check(1281, 720, area), Err(Error::Bounds));
        assert_eq!(check(198, 720, area), Err(Error::Bounds));
        assert_eq!(check(1280, 199, area), Err(Error::Bounds));
        assert_eq!(check(4098, 200, u64::MAX), Err(Error::Bounds));
        assert_eq!((scale(99), scale(125), scale(500), scale(501)), (None, Some(125), Some(500), None));
    }

    #[test]
    fn only_updates_inside_the_requested_size_are_dropped_until_reactivation() {
        use crate::rdp::frame::{Inbound, Normalizer, tests::bitmap_frame};
        use ironrdp::{graphics::image_processing::PixelFormat, pdu::geometry::InclusiveRectangle, session::image::DecodedImage};

        // R6b 재현: 644×633 화면에서 1280×720을 요청하면 xrdp가 재활성화 전에 새 크기 좌표의 비트맵을 보낸다.
        let image = DecodedImage::new(PixelFormat::RgbA32, 644, 633);
        let tile = |left: u16, top: u16| bitmap_frame(InclusiveRectangle { left, top, right: left, bottom: top + 1 });
        let normalize = |pdu: &[u8]| Normalizer::default().normalize(ironrdp::pdu::Action::FastPath, pdu, &image).map(|_| ());
        let mut resizing = Resizing::default();
        assert_eq!(resizing.skip(normalize(&tile(1000, 0))), Err(Error::OutOfImage { right: 1000, bottom: 1 }));
        resizing.requested(1280, 720);
        assert_eq!(resizing.skip(normalize(&tile(1000, 0))), Ok(None));
        assert_eq!(resizing.skip(normalize(&tile(1279, 718))), Ok(None));
        // 요청한 크기보다 큰 좌표와 형식 오류는 버리지 않는다.
        assert_eq!(resizing.skip(normalize(&tile(1280, 0))), Err(Error::OutOfImage { right: 1280, bottom: 1 }));
        assert_eq!(resizing.skip(normalize(&tile(0, 719))), Err(Error::OutOfImage { right: 0, bottom: 720 }));
        assert_eq!(resizing.skip(Err::<(), _>(Error::Bounds)), Err(Error::Bounds));
        // 해상도 변경 중에도 길이가 어긋난 클립보드 조각은 세션을 끝낸다.
        let mut inbound = Inbound::default();
        inbound.io_id = 1003;
        inbound.clipboard_id = Some(1004);
        let flags = ironrdp::pdu::rdp::vc::ChannelControlFlags::FLAG_FIRST | ironrdp::pdu::rdp::vc::ChannelControlFlags::FLAG_LAST;
        let clipboard = crate::rdp::frame::tests::channel_pdu(1004, 8, flags, &[0; 4]);
        assert_eq!(resizing.skip(inbound.check(ironrdp::pdu::Action::X224, &clipboard, &image)), Err(Error::Bounds));
        // 재활성화가 끝나면 다시 모든 범위 오류로 세션을 끝낸다.
        resizing.reactivated();
        assert_eq!(resizing.skip(normalize(&tile(1000, 0))), Err(Error::OutOfImage { right: 1000, bottom: 1 }));
    }
}
