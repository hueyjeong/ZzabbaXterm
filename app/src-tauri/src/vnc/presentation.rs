use super::{Change, Error, Rect, Size};

// 화면 하나를 패킷 하나로 IPC에 보낸다. 헤더는 프레임 종류, 순번, 화면 세대,
// 화면 크기, 사각형 순서이며 정수는 모두 little-endian이다.
pub(super) const HEADER: usize = 29;

pub(super) struct Image<'a> {
    pub kind: u8,
    pub size: Size,
    pub rect: Rect,
    pub rgba: &'a [u8],
}

impl Image<'_> {
    pub(super) fn packet(&self, sequence: u64, epoch: u64) -> Vec<u8> {
        let mut packet = Vec::with_capacity(HEADER + self.rgba.len());
        packet.push(self.kind);
        packet.extend(sequence.to_le_bytes());
        packet.extend(epoch.to_le_bytes());
        for field in [self.size.width, self.size.height, self.rect.x, self.rect.y, self.rect.size.width, self.rect.size.height] {
            packet.extend(field.to_le_bytes());
        }
        packet.extend_from_slice(self.rgba);
        packet
    }
}

#[derive(Default)]
pub(super) struct Surface {
    size: Option<Size>,
    ready: bool,
    buffer: Vec<u8>,
    copied: Vec<u8>,
}

impl Surface {
    pub(super) fn apply<'a>(&'a mut self, change: &'a Change) -> Result<Option<Image<'a>>, Error> {
        if let Change::Size(size) = change {
            self.size = Some(*size);
            self.ready = false;
            self.buffer.resize(size.pixels()? * 4, 0);
            self.copied = Vec::new();
            return Ok(None);
        }
        let size = self.size.ok_or(Error::InvalidFrame)?;
        Ok(match change {
            Change::Ready => {
                self.ready = true;
                Some(Image { kind: 1, size, rect: Rect { x: 0, y: 0, size }, rgba: &self.buffer })
            }
            Change::Pixels { rect, rgba } => {
                blit(&mut self.buffer, size, *rect, rgba);
                self.ready.then_some(Image { kind: 2, size, rect: *rect, rgba })
            }
            Change::Copy { source, destination } => {
                let width = usize::from(source.size.width) * 4;
                self.copied.resize(width * usize::from(source.size.height), 0);
                for (row, chunk) in self.copied.chunks_exact_mut(width).enumerate() {
                    let start = start(size, *source, row);
                    chunk.copy_from_slice(&self.buffer[start..start + width]);
                }
                blit(&mut self.buffer, size, *destination, &self.copied);
                Some(Image { kind: 2, size, rect: *destination, rgba: &self.copied })
            }
            Change::Size(_) => unreachable!(),
        })
    }
}

fn start(size: Size, rect: Rect, row: usize) -> usize {
    ((row + usize::from(rect.y)) * usize::from(size.width) + usize::from(rect.x)) * 4
}

fn blit(buffer: &mut [u8], size: Size, rect: Rect, rgba: &[u8]) {
    let width = usize::from(rect.size.width) * 4;
    for (row, chunk) in rgba.chunks_exact(width).enumerate() {
        let start = start(size, rect, row);
        buffer[start..start + width].copy_from_slice(chunk);
    }
}
