use crate::{connections::profile::S3, ssm::Scope};

use super::{AwsS3, Error, Failure};

pub(crate) async fn run(s3: &S3) -> Result<String, String> {
    let scope = Scope::new(s3.profile.clone(), s3.region.clone())
        .map_err(|_| "S3 시험: AWS 프로필 또는 리전이 올바르지 않습니다")?;
    let config = scope.direct_sso_config()
        .map_err(|_| "S3 시험: 공식 AWS SSO 프로필 설정이 필요합니다")?;
    let result = async {
        let verified = AwsS3::for_sso_profile_with_account(scope.region(), config).await?;
        confirm_prefix(verified, s3).await
    }.await;
    result.map_err(|error| match error.failure {
        Failure::SsoLoginRequired => "S3 시험: 선택한 AWS 프로필에 SSO 로그인이 필요합니다".into(),
        failure => format!("S3 시험 ({:?}): {failure:?}", error.stage),
    })
}

async fn confirm_prefix((client, account): (AwsS3, String), s3: &S3) -> Result<String, Error> {
    client.list_objects(&s3.bucket, &s3.prefix, None).await?;
    Ok(account)
}

#[cfg(test)]
mod tests {
    use aws_smithy_async::time::SharedTimeSource;
    use aws_smithy_runtime_api::client::http::SharedHttpClient;
    use super::super::Stage;

    use super::*;
    use crate::{ssm::{DirectSsoConfig, sso_cache_entry}, storage::tests::{Clock, caller_identity, replay, response, role_credentials}};

    #[tokio::test]
    async fn exact_prefix_is_listed_before_returning_the_sts_account() {
        let home = tempfile::tempdir().unwrap();
        let cache = home.path().join(sso_cache_entry("probe-session"));
        std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
        std::fs::write(cache, r#"{"accessToken":"fixture","expiresAt":"2099-01-01T00:00:00Z"}"#).unwrap();
        let s3 = S3 { profile: "probe-profile".into(), region: "eu-west-1".into(),
            bucket: "selected-bucket".into(), prefix: "한글//+%2F".into() };
        let account = "012345678901";
        for (listing, expected) in [
            (response(200, "<ListBucketResult><EncodingType>url</EncodingType><IsTruncated>false</IsTruncated></ListBucketResult>"), Ok(account.to_owned())),
            (response(403, "<Error><Code>AccessDenied</Code></Error>"), Err(Error::at(Stage::ListObjects, Failure::Service { status: 403, code: Some("AccessDenied".into()) }))),
        ] {
            let clock = Clock::default();
            let transport = replay([role_credentials("fixture-key", &clock, 3600), caller_identity(account), listing]);
            let config = DirectSsoConfig { account: account.into(), role: "ReadOnly".into(),
                region: "us-east-1".into(), start_url: "https://fixture.invalid/start".into(), session: Some("probe-session".into()), scopes: None };
            let verified = AwsS3::verified_with_account(&s3.region, config, home.path().to_owned(),
                SharedHttpClient::new(transport.clone()), SharedTimeSource::new(clock)).await.unwrap();
            assert_eq!(confirm_prefix(verified, &s3).await, expected);
            let requests: Vec<_> = transport.actual_requests().map(|request| request.uri().parse::<http::Uri>().unwrap()).collect();
            assert_eq!(requests.len(), 3);
            assert_eq!(requests[0].host(), Some("portal.sso.us-east-1.amazonaws.com"));
            assert_eq!(requests[1].host(), Some("sts.eu-west-1.amazonaws.com"));
            assert_eq!(requests[2].host(), Some("s3.eu-west-1.amazonaws.com"));
            assert_eq!(requests[2].path(), "/selected-bucket/");
            assert!(requests[2].query().unwrap().split('&').any(|part| part == "prefix=%ED%95%9C%EA%B8%80%2F%2F%2B%252F"));
        }
    }
}
