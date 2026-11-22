use std::sync::Mutex;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
pub(super) struct Lifetime {
    token: CancellationToken,
    gate: Mutex<()>,
}

impl Lifetime {
    pub(super) fn revoke(&self) {
        let _guard = self.gate.lock().unwrap();
        self.token.cancel();
    }

    pub(super) fn revoked(&self) -> bool { self.token.is_cancelled() }

    pub(super) async fn cancelled(&self) { self.token.cancelled().await; }

    // 각 비차단 쓰기와 철회를 같은 게이트에서 순서화한다.
    pub(super) fn live<T>(&self, act: impl FnOnce() -> T) -> Option<T> {
        let _guard = self.gate.lock().unwrap();
        (!self.revoked()).then(act)
    }
}
