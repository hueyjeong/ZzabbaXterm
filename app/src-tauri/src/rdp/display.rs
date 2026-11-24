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

/// 해상도 변경을 보낸 뒤에는 서버가 새 크기 좌표로 보낸 화면 갱신을 그리지 않고 버린다.
/// xrdp 0.9.24는 Deactivate All보다 먼저 새 크기의 비트맵을 보낸다. 그 밖의 범위 오류는 그대로 세션을 끝낸다.
pub(super) fn skip_while_resizing<T>(result: Result<T, Error>, resizing: bool) -> Result<Option<T>, Error> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(Error::Bounds) if resizing => Ok(None),
        Err(error) => Err(error),
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
    fn out_of_bounds_updates_are_dropped_only_while_a_resize_is_pending() {
        use ironrdp::pdu::geometry::InclusiveRectangle;
        use ironrdp::session::image::DecodedImage;
        use ironrdp::graphics::image_processing::PixelFormat;

        // R6b 재현: 644×633 화면에서 1280×720을 요청하면 xrdp가 재활성화 전에 새 크기 좌표의 비트맵을 보낸다.
        let image = DecodedImage::new(PixelFormat::RgbA32, 644, 633);
        let beyond = crate::rdp::frame::tests::bitmap_frame(InclusiveRectangle { left: 1000, top: 0, right: 1000, bottom: 1 });
        let action = ironrdp::pdu::Action::FastPath;
        let mut normalizer = crate::rdp::frame::Normalizer::default();
        assert_eq!(skip_while_resizing(normalizer.normalize(action, &beyond, &image).map(|_| ()), false), Err(Error::Bounds));
        assert_eq!(skip_while_resizing(normalizer.normalize(action, &beyond, &image).map(|_| ()), true), Ok(None));
        assert_eq!(skip_while_resizing(Err::<(), _>(Error::Protocol), true), Err(Error::Protocol));
        assert_eq!(skip_while_resizing(Ok(7), true), Ok(Some(7)));
    }
}
