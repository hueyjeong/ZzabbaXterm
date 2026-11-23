//! 명시적 승격 실행 한 건의 고정 정보, 1회 승인, 백엔드 askpass 교환이다.
//!
//! 셸 문자열은 받지 않는다. 승인 창은 고정한 대상·계정·절대 실행 파일·인자 배열을
//! 그대로 표시하고, 그 자식의 `sudo -A`에만 같은 승인을 결합한다.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use portable_pty::{Child, SlavePty};
use tokio::time::Instant;

pub(crate) use crate::local_auth::Error;

/// 승인 30초. 승인 뒤 교환은 `sudo -A`의 askpass 시작을 별도로 기다린다.
const TTL: Duration = Duration::from_secs(30);

/// 승인 대상은 문서 세대와 요청 ID에 묶인다. 다른 탭·다른 요청에는 쓰이지 않는다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Document {
    pub(crate) owner: String,
    pub(crate) document: u64,
    pub(crate) request: String,
}

/// 승인 창과 실행 자식이 함께 쓰는 고정 정보다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Request {
    pub(crate) target: String,
    pub(crate) caller: String,
    pub(crate) run_as: String,
    pub(crate) program: PathBuf,
    pub(crate) args: Vec<String>,
    pub(crate) item: String,
}

impl Request {
    pub(crate) fn new(caller: String, program: PathBuf, args: Vec<String>, item: String) -> Self {
        Self {
            target: "내 PC".into(),
            caller,
            run_as: "root".into(),
            program,
            args,
            item,
        }
    }
}

#[must_use]
pub(crate) struct Pending {
    document: Document,
    request: Request,
    expires: Instant,
}

/// 승인에서 실행까지 복제하거나 다시 결합할 수 없는 소유권이다.
#[must_use]
pub(crate) struct Approval(Pending);

impl Pending {
    pub(crate) fn request(document: Document, request: Request) -> Result<Self, Error> {
        validate_document(&document)?;
        validate_request(&request)?;
        Ok(Self {
            document,
            request,
            expires: Instant::now() + TTL,
        })
    }

    #[cfg(test)]
    pub(crate) fn details(&self) -> (&Document, &Request) {
        (&self.document, &self.request)
    }

    pub(crate) fn approve(
        self,
        current_document: &Document,
        current_request: &Request,
    ) -> Result<Approval, Error> {
        self.check(current_document, current_request)?;
        Ok(Approval(self))
    }

    fn check(&self, document: &Document, request: &Request) -> Result<(), Error> {
        if &self.document != document || &self.request != request {
            return Err(Error::InvalidApproval);
        }
        if Instant::now() >= self.expires {
            return Err(Error::Expired);
        }
        Ok(())
    }
}

impl Approval {
    /// 로컬 승격 실행은 승인한 실행 파일·인자 배열 그대로 `sudo -A --`에 넘긴다.
    pub(crate) fn spawn_local(
        self,
        document: &Document,
        request: &Request,
        slave: &dyn SlavePty,
        helper: crate::local_auth::TrustedFile,
        sudo: crate::local_auth::TrustedFile,
        vault: crate::local_auth::Bitwarden,
    ) -> Result<(Box<dyn Child + Send + Sync>, crate::local_auth::Exchange), Error> {
        self.0.check(document, request)?;
        let (document, request) = self.0.into_parts();
        spawn_local(&document, &request, slave, helper, sudo, vault)
    }

}

impl Pending {
    fn into_parts(self) -> (Document, Request) {
        let Pending {
            document, request, ..
        } = self;
        (document, request)
    }
}

/// PTY 감독자에게 넘기는 승인된 승격 실행이다. 승인 정보와 실행 정보를 함께 보관한다.
pub(crate) struct Launch {
    approval: Approval,
    document: Document,
    request: Request,
    helper: crate::local_auth::TrustedFile,
    sudo: crate::local_auth::TrustedFile,
    vault: crate::local_auth::Bitwarden,
}

impl Launch {
    pub(crate) fn new(
        approval: Approval,
        document: Document,
        request: Request,
        helper: crate::local_auth::TrustedFile,
        sudo: crate::local_auth::TrustedFile,
        vault: crate::local_auth::Bitwarden,
    ) -> Self {
        Self {
            approval,
            document,
            request,
            helper,
            sudo,
            vault,
        }
    }

    /// 실행 직전에 승인 대상과 요청을 다시 대조한 뒤 자식 하나를 시작한다.
    pub(crate) fn spawn(
        self,
        slave: &dyn SlavePty,
    ) -> Result<(Box<dyn Child + Send + Sync>, crate::local_auth::Exchange), Error> {
        let Self {
            approval,
            document,
            request,
            helper,
            sudo,
            vault,
        } = self;
        // PTY 감독자 스레드에는 tokio 컨텍스트가 없다. 교환 작업을 Tauri 런타임에 붙인다.
        tauri::async_runtime::block_on(async move {
            approval.spawn_local(&document, &request, slave, helper, sudo, vault)
        })
    }
}

fn spawn_local(
    _document: &Document,
    request: &Request,
    slave: &dyn SlavePty,
    helper: crate::local_auth::TrustedFile,
    sudo: crate::local_auth::TrustedFile,
    vault: crate::local_auth::Bitwarden,
) -> Result<(Box<dyn Child + Send + Sync>, crate::local_auth::Exchange), Error> {
    crate::local_auth::spawn_sudo(
        &request.program,
        &request.args,
        helper,
        sudo,
        vault,
        &request.item,
        |command| {
            slave
                .spawn_command(command)
                .map_err(|_| Error::ChildUnavailable)
        },
    )
}

/// 검증한 helper와 sudo 실행기가 있을 때만 승인 창을 연다. 없으면 직접 입력만 허용한다.
pub(crate) fn binaries() -> Result<
    (crate::local_auth::TrustedFile, crate::local_auth::TrustedFile),
    Error,
> {
    Ok((
        crate::local_auth::installed_helper()?,
        crate::local_auth::sudo_program()?,
    ))
}

/// 승인 창에는 인자 경계까지 그대로 표시한다. 셸 문자열로 다시 합치지 않는다.
pub(crate) fn message(document: &Document, request: &Request) -> String {
    let mut lines = vec![
        format!("대상: {}", request.target),
        format!("호출 계정: {}", request.caller),
        format!("실행 계정: {}", request.run_as),
        format!("실행 파일: {}", request.program.display()),
        format!("금고 항목: {}", request.item),
        format!("요청 ID: {}", document.request),
    ];
    if request.args.is_empty() {
        lines.push("인자: (없음)".into());
    } else {
        lines.push(format!("인자 {}개:", request.args.len()));
        lines.extend(
            request
                .args
                .iter()
                .enumerate()
                .map(|(index, argument)| format!("  {}. {argument}", index + 1)),
        );
    }
    lines.join("\n")
}

/// 요청은 사용자가 명시적으로 고른 실행 파일·인자·금고 항목만 받는다.
pub(crate) fn validate_request(request: &Request) -> Result<(), Error> {
    if [&request.target, &request.caller, &request.run_as]
        .into_iter()
        .any(|value| !valid_text(value))
        || request.run_as != "root"
        || !request.program.is_absolute()
        || !valid_text(&request.program.to_string_lossy())
        || request.args.len() > 256
        || request
            .args
            .iter()
            .any(|argument| argument.len() > 8192 || argument.contains('\0') || argument.chars().any(char::is_control))
        || !crate::local_auth::valid_item(&request.item)
    {
        return Err(Error::InvalidRequest);
    }
    Ok(())
}

fn validate_document(document: &Document) -> Result<(), Error> {
    (valid_text(&document.owner)
        && document.request.len() == 32
        && document.request.bytes().all(|byte| byte.is_ascii_hexdigit()))
    .then_some(())
    .ok_or(Error::InvalidApproval)
}

fn valid_text(value: &str) -> bool {
    !value.trim().is_empty() && value.chars().all(|c| !c.is_control())
}

/// 승격 요청을 만든 호출 계정이다. 프런트가 보낸 값이 아니라 OS에서 읽는다.
pub(crate) fn account() -> Option<String> {
    #[cfg(unix)]
    {
        use std::ffi::CStr;
        let uid = unsafe { libc::geteuid() };
        let mut buffer = [0u8; 4096];
        let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut result = std::ptr::null_mut();
        // SAFETY: buffer and entry are writable for the call, and result receives an entry pointer.
        let status = unsafe {
            libc::getpwuid_r(
                uid,
                entry.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if status != 0 || result.is_null() {
            return None;
        }
        // SAFETY: getpwuid_r filled entry with a NUL-terminated name inside buffer.
        let entry = unsafe { entry.assume_init() };
        // SAFETY: pw_name points into the still-live buffer.
        unsafe { CStr::from_ptr(entry.pw_name) }
            .to_str()
            .ok()
            .map(str::to_owned)
    }
    #[cfg(windows)]
    {
        std::env::var("USERNAME").ok()
    }
}

/// 요청 ID는 승인 한 건과 무작위로 묶인다.
pub(crate) fn request_id() -> Option<String> {
    use std::fmt::Write;
    let mut random = [0u8; 16];
    getrandom::fill(&mut random).ok()?;
    let mut id = String::with_capacity(32);
    for byte in random {
        write!(id, "{byte:02x}").ok()?;
    }
    Some(id)
}

/// 실행 파일은 절대 경로로 고정한다. 표시와 실행이 같은 경로를 쓰도록 정규화한다.
pub(crate) fn canonical_program(program: &str) -> Option<PathBuf> {
    let path = Path::new(program);
    if !path.is_absolute() || program.contains('\0') || program.chars().any(char::is_control) {
        return None;
    }
    let canonical = path.canonicalize().ok()?;
    canonical.is_file().then_some(canonical)
}

/// 승인 뒤 금고 오류를 사용자 상태로 전달한다.
pub(crate) fn failure(error: Error) -> Option<&'static str> {
    crate::local_auth::sudo_failure(error)
}

#[cfg(test)]
mod tests;
