use std::{
    error::Error as _,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use aws_smithy_checksums::body::validate;
use aws_smithy_runtime::client::http::body::content_length_enforcement::ContentLengthError;
use aws_smithy_runtime_api::{
    box_error::BoxError,
    client::{
        interceptors::{
            Intercept,
            context::{
                BeforeDeserializationInterceptorContextRef,
                BeforeSerializationInterceptorContextRef,
            },
        },
        runtime_components::RuntimeComponents,
        unit_of_work::checksums::{
            ChecksumEvent, ChecksumEventKind, ChecksumSkip, ObserveResponseChecksum,
            SharedResponseChecksumObserver,
        },
    },
};
use aws_smithy_types::{checksum_config::ResponseChecksumValidation, config_bag::ConfigBag};
use sha2::{Digest, Sha256};
use crate::editor::storage_files::{DownloadPublicationError, DownloadTarget};
use tokio::{fs::File, io::AsyncWriteExt, sync::watch};

use super::{DownloadIntegrity, Downloaded, Error, Failure, S3Operations, Stage, single_attempt};

pub(super) async fn download(
    operations: &S3Operations<impl std::borrow::Borrow<aws_sdk_s3::Client>>,
    bucket: &str,
    key: &str,
    target: &DownloadTarget,
    closed: &watch::Sender<bool>,
) -> Result<Downloaded, Error> {
    let checksum = ChecksumStatus::default();
    // S3 대기는 즉시 취소하고, 시작한 로컬 쓰기는 정리 전에 끝낸다.
    let mut cancellation = closed.subscribe();
    let request = operations.client.borrow()
        .get_object()
        .bucket(bucket)
        .key(key)
        .customize()
        .config_override(
            single_attempt().response_checksum_validation(ResponseChecksumValidation::WhenRequired),
        )
        .mutate_request(|request| {
            request
                .headers_mut()
                .insert("x-amz-checksum-mode", "ENABLED");
        })
        .interceptor(checksum.clone())
        .send();
    let output = tokio::select! {
        biased;
        _ = cancellation.wait_for(|closed| *closed) => {
            return Err(Error::at(Stage::GetObject, Failure::Request));
        }
        result = request => result.map_err(|err| operations.sdk_error(Stage::GetObject, err))?,
    };
    let expected = output.content_length().and_then(|n| u64::try_from(n).ok());
    let length = |bytes| Error {
        stage: Stage::DownloadBody,
        failure: Failure::Length { expected },
        bytes,
    };
    let expected = expected.ok_or_else(|| length(0))?;
    if checksum.invalid.load(Ordering::Relaxed) {
        return Err(Error::at(Stage::DownloadBody, Failure::Checksum));
    }

    let mut staged = target.stage().map_err(|err| local_error(Stage::DownloadWrite, err, 0))?;
    let mut file = File::from_std(staged.writer().map_err(|err| local_error(Stage::DownloadWrite, err, 0))?);
    let mut digest = Sha256::new();
    let mut body = output.body;
    let mut bytes = 0;
    let result = async {
    loop {
        let next = tokio::select! {
            biased;
            _ = cancellation.wait_for(|closed| *closed) => Err(Error {
                stage: Stage::DownloadBody,
                failure: Failure::Request,
                bytes,
            }),
            chunk = body.next() => Ok(chunk),
        };
        let chunk = match next {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break,
            Err(error) => return Err(error),
        };
        let chunk = chunk.map_err(|err| {
            let mut cause = err.source();
            while let Some(source) = cause {
                if source.is::<validate::Error>() {
                    return Error {
                        stage: Stage::DownloadBody,
                        failure: Failure::Checksum,
                        bytes,
                    };
                }
                if source.is::<ContentLengthError>() {
                    return length(bytes);
                }
                cause = source.source();
            }
            Error {
                stage: Stage::DownloadBody,
                failure: Failure::BodyRead,
                bytes,
            }
        })?;
        if expected - bytes < chunk.len() as u64 {
            return Err(length(bytes));
        }
        file.write_all(&chunk)
            .await
            .map_err(|err| local_error(Stage::DownloadWrite, err, bytes))?;
        digest.update(&chunk);
        bytes += chunk.len() as u64;
    }
    if bytes != expected {
        return Err(length(bytes));
    }
    file.flush()
        .await
        .map_err(|err| local_error(Stage::DownloadWrite, err, bytes))
    }.await;
    // Wait for the Tokio worker to release its file handle, including on body errors.
    drop(file.into_std().await);
    result?;
    staged.commit(closed, digest.finalize().into())
        .map_err(|error| match error {
            DownloadPublicationError::Io(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                Error { stage: Stage::DownloadCommit, failure: Failure::Request, bytes }
            }
            DownloadPublicationError::Io(error) => local_error(Stage::DownloadCommit, error, bytes),
            DownloadPublicationError::Unconfirmed(path) => Error {
                stage: Stage::DownloadCommit,
                failure: Failure::RecoveryUnconfirmed {
                    recovery_path: path.to_str().map_or_else(|| format!("{path:?}"), str::to_owned),
                },
                bytes,
            },
        })?;
    Ok(Downloaded {
        bytes,
        integrity: if checksum.matched.load(Ordering::Relaxed) {
            DownloadIntegrity::SdkChecksumVerified
        } else {
            DownloadIntegrity::LengthOnly
        },
    })
}

#[derive(Clone, Debug, Default)]
struct ChecksumStatus {
    matched: Arc<AtomicBool>,
    invalid: Arc<AtomicBool>,
}

impl Intercept for ChecksumStatus {
    fn name(&self) -> &'static str {
        "S3DownloadChecksumStatus"
    }

    fn read_before_serialization(
        &self,
        _: &BeforeSerializationInterceptorContextRef<'_>,
        _: &RuntimeComponents,
        cfg: &mut ConfigBag,
    ) -> Result<(), BoxError> {
        cfg.interceptor_state()
            .store_append(SharedResponseChecksumObserver::new(self.clone()));
        Ok(())
    }

    fn read_after_transmit(
        &self,
        context: &BeforeDeserializationInterceptorContextRef<'_>,
        _: &RuntimeComponents,
        cfg: &mut ConfigBag,
    ) -> Result<(), BoxError> {
        self.matched.store(false, Ordering::Relaxed);
        self.invalid.store(false, Ordering::Relaxed);
        // 고정 SDK는 -N 접미사만 보고 합성을 판별하므로 체크섬 유형도 확인한다.
        let validation = if context
            .response()
            .headers()
            .get("x-amz-checksum-type")
            .is_none_or(|kind| kind == "FULL_OBJECT")
        {
            ResponseChecksumValidation::WhenSupported
        } else {
            ResponseChecksumValidation::WhenRequired
        };
        cfg.interceptor_state().store_put(validation);
        Ok(())
    }
}

impl ObserveResponseChecksum for ChecksumStatus {
    fn on_event(&self, event: &ChecksumEvent<'_>) {
        match event.kind() {
            ChecksumEventKind::Matched(_) => self.matched.store(true, Ordering::Relaxed),
            ChecksumEventKind::NotValidated(skip)
                if skip.reason() == ChecksumSkip::InvalidChecksumHeader =>
            {
                self.invalid.store(true, Ordering::Relaxed);
            }
            _ => {}
        }
    }
}

fn local_error(stage: Stage, error: std::io::Error, bytes: u64) -> Error {
    Error {
        stage,
        failure: Failure::LocalIo {
            error_kind: format!("{:?}", error.kind()),
        },
        bytes,
    }
}
