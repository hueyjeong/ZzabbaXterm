use super::{CliError, valid_region};
use std::{
    collections::{HashMap, HashSet},
    env,
    fs::File,
    io::{self, Read},
    path::{Path, PathBuf},
};

const CONFIG_LIMIT: u64 = 1024 * 1024;

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct DirectSsoConfig {
    pub(crate) account: String,
    pub(crate) role: String,
    pub(crate) region: String,
    pub(crate) start_url: String,
    pub(crate) session: Option<String>,
    /// sso-session의 `sso_registration_scopes`. 재로그인 스냅샷에 빠지면 갱신 토큰 없이 등록된다.
    pub(crate) scopes: Option<String>,
}

impl DirectSsoConfig {
    #[cfg(test)]
    pub(super) fn cli_ini(&self) -> String {
        self.cli_ini_for("default")
    }

    pub(super) fn cli_ini_for(&self, profile: &str) -> String {
        let Self { account, role, region, start_url, session, scopes } = self;
        let mut config = format!("[{}]\nsso_account_id = {account}\nsso_role_name = {role}\n", section(profile));
        if let Some(name) = session {
            config.push_str(&format!("sso_session = {name}\n[sso-session \"{name}\"]\n"));
            if let Some(scopes) = scopes {
                config.push_str(&format!("sso_registration_scopes = {scopes}\n"));
            }
        }
        config.push_str(&format!("sso_region = {region}\nsso_start_url = {start_url}\n"));
        config
    }

    pub(crate) fn cache_key(&self) -> &str {
        self.session.as_deref().unwrap_or(&self.start_url)
    }
}

fn section(profile: &str) -> String {
    if profile == "default" { "default".to_owned() } else { format!("profile \"{}\"", profile.replace('"', "\\\"")) }
}

/// `aws login`으로 만든 프로필. 자격은 로그인 캐시의 `login_session` 항목에서 나온다.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct LoginConfig {
    pub(crate) session: String,
    pub(crate) region: Option<String>,
}

/// 앱이 내보낼 수 있는 프로필 인증 방식.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum ProfileAuth {
    Sso(DirectSsoConfig),
    Login(LoginConfig),
}

impl ProfileAuth {
    /// 자격 갱신에는 리전이 필요하다. 프로필에 리전이 없으면 대상 리전을 쓴다.
    pub(super) fn cli_ini_for(&self, profile: &str, fallback_region: &str) -> String {
        match self {
            Self::Sso(config) => config.cli_ini_for(profile),
            Self::Login(config) => format!(
                "[{}]\nlogin_session = {}\nregion = {}\n",
                section(profile), config.session, config.login_region(fallback_region),
            ),
        }
    }
}

impl LoginConfig {
    pub(super) fn login_region<'a>(&'a self, fallback: &'a str) -> &'a str {
        self.region.as_deref().unwrap_or(fallback)
    }
}

/// 사용자 AWS CLI와 같은 로그인 캐시 디렉터리.
pub(super) fn login_cache_dir() -> Result<PathBuf, CliError> {
    path("AWS_LOGIN_CACHE_DIRECTORY", "login/cache")
}

pub(super) struct Profiles {
    known: HashSet<String>,
    direct: HashMap<String, DirectSsoConfig>,
    login: HashMap<String, LoginConfig>,
}

impl Profiles {
    pub(super) fn load() -> Result<Self, CliError> {
        let config = read(&path("AWS_CONFIG_FILE", "config")?)?;
        let credentials = read(&path("AWS_SHARED_CREDENTIALS_FILE", "credentials")?)?;
        Self::parse(&config, &credentials)
    }

    pub(super) fn require(&self, name: &str) -> Result<(), CliError> {
        if self.direct.contains_key(name) || self.login.contains_key(name) {
            Ok(())
        } else if self.known.contains(name) {
            Err(CliError::UnsupportedProfile)
        } else {
            Err(CliError::ProfileNotFound)
        }
    }

    pub(super) fn is_login(&self, name: &str) -> bool {
        self.login.contains_key(name)
    }

    pub(super) fn auth(&self, name: &str) -> Result<ProfileAuth, CliError> {
        self.require(name)?;
        Ok(match self.direct.get(name) {
            Some(config) => ProfileAuth::Sso(config.clone()),
            None => ProfileAuth::Login(self.login[name].clone()),
        })
    }

    /// S3처럼 SSO 토큰을 직접 쓰는 기능은 SSO 프로필만 받는다.
    pub(super) fn take_sso_config(&mut self, name: &str) -> Result<DirectSsoConfig, CliError> {
        self.require(name)?;
        self.direct.remove(name).ok_or(CliError::UnsupportedProfile)
    }

    fn parse(config: &str, credentials: &str) -> Result<Self, CliError> {
        let config = ini(config)?;
        if config.contains_key("plugins") {
            return Err(CliError::InvalidConfiguration);
        }
        let credentials = ini(credentials)?;
        let mut profiles = HashMap::new();
        let mut sessions = HashMap::new();
        for (section, mut options) in config {
            let (map, name) = if section == "default" {
                (&mut profiles, "default".to_owned())
            } else if let Some(map) = if section.starts_with("profile") {
                Some(&mut profiles)
            } else if section.starts_with("sso-session") {
                Some(&mut sessions)
            } else {
                None
            } {
                let Some(parts) = shlex::split(section) else {
                    continue;
                };
                let [_, name] = parts.as_slice() else {
                    continue;
                };
                // Python and the Rust lexer differ on shell comments and escapes.
                options.unsupported |= section.contains(['#', '\\']);
                (map, name.clone())
            } else {
                continue;
            };
            map.entry(name)
                .and_modify(|existing: &mut Section<'_>| existing.unsupported = true)
                .or_insert(options);
        }
        let mut known = HashSet::new();
        let mut direct = HashMap::new();
        let mut login = HashMap::new();
        for (name, mut profile) in profiles {
            if let Some(shared) = credentials.get(name.as_str()) {
                // CLI는 login_session을 설정 파일에서만 읽는다.
                profile.unsupported |= shared.unsupported || shared.options.contains_key("login_session");
                profile.options.extend(
                    shared
                        .options
                        .iter()
                        .map(|(key, value)| (key.clone(), *value)),
                );
            }
            if !profile.unsupported
                && !name.contains(['#', '\\'])
                && let Some(config) = direct_config(&profile.options, &sessions)
            {
                direct.insert(name.clone(), config);
            } else if !profile.unsupported
                && !name.contains(['#', '\\'])
                && let Some(config) = login_config(&profile.options)
            {
                login.insert(name.clone(), config);
            }
            known.insert(name);
        }
        known.extend(credentials.keys().map(|name| (*name).to_owned()));
        Ok(Self { known, direct, login })
    }
}

fn direct_config(
    options: &Options<'_>,
    sessions: &HashMap<String, Section<'_>>,
) -> Option<DirectSsoConfig> {
    if options.keys().any(|key| {
        key.starts_with("aws_")
            || matches!(
                key.as_str(),
                "credential_process"
                    | "credential_source"
                    | "source_profile"
                    | "role_arn"
                    | "web_identity_token_file"
                    | "login_session"
            )
    }) {
        return None;
    }
    let account = options.get("sso_account_id")?;
    let role = options.get("sso_role_name")?;
    if account.len() != 12 || !account.bytes().all(|b| b.is_ascii_digit()) || role.is_empty() {
        return None;
    }
    let name = options.get("sso_session").copied();
    let settings = if let Some(name) = name {
        if name.is_empty() || name.contains(['#', '\\', '\'', '"', '[', ']']) {
            return None;
        }
        let settings = sessions.get(name)?;
        if settings.unsupported {
            return None;
        }
        let settings = &settings.options;
        if ["sso_start_url", "sso_region"].into_iter().any(|key| {
            options
                .get(key)
                .is_some_and(|value| Some(value) != settings.get(key))
        }) {
            return None;
        }
        settings
    } else {
        options
    };
    let region = settings.get("sso_region")?;
    let url = settings.get("sso_start_url")?;
    // 빈 값은 범위 없음과 같다. 그 밖에는 스냅샷 ini에 그대로 쓸 수 있는 문자만 받는다.
    let scopes = match name.and_then(|_| settings.get("sso_registration_scopes")).filter(|scopes| !scopes.is_empty()) {
        Some(scopes) if scopes.bytes().all(|b| b.is_ascii_alphanumeric() || b":,._- ".contains(&b)) => {
            Some((*scopes).to_owned())
        }
        Some(_) => return None,
        None => None,
    };
    if !valid_region(region)
        || !url
            .strip_prefix("https://")
            .is_some_and(|suffix| !suffix.is_empty())
        || url.chars().any(char::is_whitespace)
    {
        return None;
    }
    Some(DirectSsoConfig {
        account: (*account).to_owned(),
        role: (*role).to_owned(),
        region: (*region).to_owned(),
        start_url: (*url).to_owned(),
        session: name.map(str::to_owned),
        scopes,
    })
}

fn login_config(options: &Options<'_>) -> Option<LoginConfig> {
    // 다른 자격 공급자 키가 함께 있으면 CLI가 고르는 공급자를 앱이 재현하지 않는다.
    if options.keys().any(|key| {
        key.starts_with("aws_")
            || key.starts_with("sso_")
            || matches!(
                key.as_str(),
                "credential_process" | "credential_source" | "source_profile" | "role_arn" | "web_identity_token_file"
            )
    }) {
        return None;
    }
    let session = options.get("login_session")?;
    let valid = session.strip_prefix("arn:").is_some_and(|rest| !rest.is_empty())
        && session.len() <= 2048
        && session.bytes().all(|b| b.is_ascii_alphanumeric() || b"/:+=,.@_-".contains(&b));
    if !valid {
        return None;
    }
    let region = match options.get("region") {
        Some(region) if valid_region(region) => Some((*region).to_owned()),
        Some(_) => return None,
        None => None,
    };
    Some(LoginConfig { session: (*session).to_owned(), region })
}

type Options<'a> = HashMap<String, &'a str>;

#[derive(Default)]
struct Section<'a> {
    options: Options<'a>,
    unsupported: bool,
}

fn ini(text: &str) -> Result<HashMap<&str, Section<'_>>, CliError> {
    let mut sections = HashMap::new();
    let mut current: Option<&mut Section<'_>> = None;
    let mut option_indent = None;
    let mut nested = false;
    for line in text.split(['\r', '\n']) {
        let value = line.trim_start_matches(python_space);
        let indent = line[..line.len() - value.len()].chars().count();
        let value = value.trim_end_matches(python_space);
        if value.is_empty() || value.starts_with(['#', ';']) {
            continue;
        }
        // ConfigParser compares character indices (a tab counts as one), then checks headers.
        if let Some(section) = current.as_deref_mut()
            && option_indent.is_some_and(|previous| indent > previous)
        {
            if nested && !value.contains('=') {
                // Botocore rejects a malformed nested mapping before selecting a profile.
                return Err(CliError::InvalidConfiguration);
            }
            section.unsupported |= !nested || value.starts_with('[');
            continue;
        }
        if let Some((header, suffix)) = value
            .strip_prefix('[')
            .and_then(|rest| rest.rsplit_once(']'))
            && !header.is_empty()
        {
            if header == "DEFAULT" || sections.contains_key(header) {
                return Err(CliError::InvalidConfiguration);
            }
            let suffix = suffix.trim_start_matches(python_space);
            let unsupported = header.contains([']', '['])
                || !(suffix.is_empty() || suffix.starts_with(['#', ';']));
            sections.insert(
                header,
                Section {
                    unsupported,
                    ..Section::default()
                },
            );
            current = sections.get_mut(header);
            option_indent = None;
            continue;
        }
        let (key, value) = value
            .split_once(['=', ':'])
            .ok_or(CliError::InvalidConfiguration)?;
        let key = key.trim_end_matches(python_space);
        let section = current
            .as_deref_mut()
            .ok_or(CliError::InvalidConfiguration)?;
        if key.is_empty()
            || section
                .options
                .insert(key.to_ascii_lowercase(), value.trim_matches(python_space))
                .is_some()
        {
            return Err(CliError::InvalidConfiguration);
        }
        // Python lowercases Unicode option names. Only ASCII names enter the provider gate.
        section.unsupported |= !key.is_ascii();
        nested = value.trim_matches(python_space).is_empty();
        option_indent = Some(indent);
    }
    Ok(sections)
}

fn python_space(c: char) -> bool {
    c.is_whitespace() || matches!(c, '\u{1c}'..='\u{1f}')
}

fn path(var: &str, default: &str) -> Result<PathBuf, CliError> {
    let home = || std::env::home_dir().ok_or(CliError::InvalidConfiguration);
    let Some(value) = env::var_os(var) else {
        return Ok(home()?.join(".aws").join(default));
    };
    let value = value.to_str().ok_or(CliError::InvalidConfiguration)?;
    if value.is_empty() || value.contains('$') || (cfg!(windows) && value.contains('%')) {
        return Err(CliError::InvalidConfiguration);
    }
    if let Some(relative) = value
        .strip_prefix("~/")
        .or_else(|| cfg!(windows).then(|| value.strip_prefix("~\\")).flatten())
    {
        return Ok(home()?.join(relative));
    }
    if value.starts_with('~') {
        return Err(CliError::InvalidConfiguration);
    }
    Ok(value.into())
}

fn read(path: &Path) -> Result<String, CliError> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(String::new()),
        Err(_) => return Err(CliError::InvalidConfiguration),
    };
    if !file
        .metadata()
        .map_err(|_| CliError::InvalidConfiguration)?
        .is_file()
    {
        return Err(CliError::InvalidConfiguration);
    }
    let mut bytes = Vec::new();
    file.take(CONFIG_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| CliError::InvalidConfiguration)?;
    if bytes.len() as u64 > CONFIG_LIMIT {
        return Err(CliError::InvalidConfiguration);
    }
    String::from_utf8(bytes).map_err(|_| CliError::InvalidConfiguration)
}

#[cfg(test)]
mod tests;
