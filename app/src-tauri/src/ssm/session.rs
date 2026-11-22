use super::{AwsCli, CancellationToken, CliError, CommandSpec, OnlineNode, Scope, decode};
use serde::Deserialize;

// 각 요청을 앱 세션에 보관하고 start-session은 요청당 한 번만 호출한다.
pub struct StartRequest {
    scope: Scope,
    target: String,
    reason: String,
    owner: String,
    command_issued: bool,
}

#[derive(Clone, Copy)]
pub enum SessionLookup {
    Active,
    History,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VerificationError {
    Unconfirmed,
}

pub struct VerifiedSession {
    session_id: String,
    terminated: bool,
}

impl VerifiedSession {
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn is_terminated(&self) -> bool {
        self.terminated
    }
}

impl AwsCli {
    pub async fn prepare_start(
        &self,
        scope: &Scope,
        node: &OnlineNode,
        cancel: &CancellationToken,
    ) -> Result<StartRequest, CliError> {
        if &node.scope != scope {
            return Err(CliError::InvalidTarget);
        }
        super::require_direct_profile(scope.profile())?;
        let owner = caller_arn(scope, cancel).await?;
        let mut nonce = [0; 16];
        getrandom::fill(&mut nonce).map_err(|_| CliError::RandomUnavailable)?;
        Ok(StartRequest {
            scope: scope.clone(),
            target: node.instance_id.clone(),
            reason: format!("zzabbaxterm-{:032x}", u128::from_be_bytes(nonce)),
            owner,
            command_issued: false,
        })
    }

    pub async fn verify_session(
        &self,
        request: &StartRequest,
        current_scope: &Scope,
        lookup: SessionLookup,
        cancel: &CancellationToken,
    ) -> Result<VerifiedSession, VerificationError> {
        use VerificationError::Unconfirmed;
        if !request.command_issued || *current_scope != request.scope {
            return Err(Unconfirmed);
        }
        super::require_direct_profile(current_scope.profile()).map_err(|_| Unconfirmed)?;
        let caller = caller_arn(current_scope, cancel)
            .await
            .map_err(|_| Unconfirmed)?;
        if caller != request.owner {
            return Err(Unconfirmed);
        }
        let result = request.lookup_command(lookup).capture(cancel).await;
        request.verify_result(current_scope, &caller, result)
    }
}

impl StartRequest {
    pub fn take_start_command(&mut self) -> Result<Option<CommandSpec>, CliError> {
        if self.command_issued {
            return Ok(None);
        }
        super::require_direct_profile(self.scope.profile())?;
        Ok(self.issue_start_command())
    }

    fn issue_start_command(&mut self) -> Option<CommandSpec> {
        if std::mem::replace(&mut self.command_issued, true) {
            return None;
        }
        Some(self.scope.command(
            "ssm",
            "start-session",
            &["--target", &self.target, "--reason", &self.reason],
        ))
    }

    pub fn target(&self) -> &str {
        &self.target
    }

    fn lookup_command(&self, lookup: SessionLookup) -> CommandSpec {
        let state = match lookup {
            SessionLookup::Active => "Active",
            SessionLookup::History => "History",
        };
        self.scope.command(
            "ssm",
            "describe-sessions",
            &[
                "--state",
                state,
                "--filters",
                &format!("key=Target,value={}", self.target),
            ],
        )
    }

    fn verify_result(
        &self,
        current_scope: &Scope,
        caller: &str,
        result: Result<super::Captured, CliError>,
    ) -> Result<VerifiedSession, VerificationError> {
        use VerificationError::Unconfirmed;
        if *current_scope != self.scope || caller != self.owner {
            return Err(Unconfirmed);
        }
        let output = result
            .and_then(super::Captured::success)
            .map_err(|_| Unconfirmed)?;
        let inventory: SessionInventory = decode(&output).map_err(|_| Unconfirmed)?;
        if inventory.next_token.is_some() {
            return Err(Unconfirmed);
        }
        let mut verified = None;
        for record in &inventory.sessions {
            if record.reason.as_deref() != Some(self.reason.as_str()) {
                continue;
            }
            if record.owner.as_deref() != Some(caller)
                || record.target.as_deref() != Some(self.target.as_str())
                || verified.is_some()
            {
                return Err(Unconfirmed);
            }
            let id = record
                .session_id
                .as_deref()
                .filter(|id| valid_session_id(id))
                .ok_or(Unconfirmed)?;
            verified = Some((id, record.status.as_deref() == Some("Terminated")));
        }
        let (id, terminated) = verified.ok_or(Unconfirmed)?;
        if inventory
            .sessions
            .iter()
            .filter(|record| record.session_id.as_deref() == Some(id))
            .count()
            != 1
        {
            return Err(Unconfirmed);
        }
        Ok(VerifiedSession {
            session_id: id.to_owned(),
            terminated,
        })
    }
}

fn identity_command(scope: &Scope) -> CommandSpec {
    scope.command("sts", "get-caller-identity", &[])
}

async fn caller_arn(scope: &Scope, cancel: &CancellationToken) -> Result<String, CliError> {
    parse_caller_arn(&identity_command(scope).capture(cancel).await?.success()?)
}

fn parse_caller_arn(bytes: &[u8]) -> Result<String, CliError> {
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Identity {
        arn: String,
    }
    let Identity { arn } = decode(bytes)?;
    let mut fields = arn.splitn(6, ':');
    let [
        Some("arn"),
        Some(partition),
        Some("iam" | "sts"),
        Some(""),
        Some(account),
        Some(resource),
    ] = std::array::from_fn(|_| fields.next())
    else {
        return Err(CliError::InvalidResponse);
    };
    if !partition.starts_with("aws")
        || account.len() != 12
        || !account.bytes().all(|b| b.is_ascii_digit())
        || resource.is_empty()
        || arn.len() > 2048
        || arn.chars().any(char::is_control)
    {
        return Err(CliError::InvalidResponse);
    }
    Ok(arn)
}

fn valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 96
        && !id.starts_with('-')
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_+=,.@-".contains(&b))
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SessionInventory {
    sessions: Vec<SessionRecord>,
    next_token: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SessionRecord {
    session_id: Option<String>,
    target: Option<String>,
    owner: Option<String>,
    reason: Option<String>,
    status: Option<String>,
}

#[cfg(test)]
mod tests;
