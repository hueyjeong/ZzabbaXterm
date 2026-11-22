use super::{CliError, valid_region};
use std::{
    collections::{HashMap, HashSet},
    env,
    fs::File,
    io::{self, Read},
    path::{Path, PathBuf},
};

const CONFIG_LIMIT: u64 = 1024 * 1024;

pub(super) struct Profiles {
    known: HashSet<String>,
    direct: HashSet<String>,
}

impl Profiles {
    pub(super) fn load() -> Result<Self, CliError> {
        let config = read(&path("AWS_CONFIG_FILE", "config")?)?;
        let credentials = read(&path("AWS_SHARED_CREDENTIALS_FILE", "credentials")?)?;
        Self::parse(&config, &credentials)
    }

    pub(super) fn require(&self, name: &str) -> Result<(), CliError> {
        if self.direct.contains(name) {
            Ok(())
        } else if self.known.contains(name) {
            Err(CliError::UnsupportedProfile)
        } else {
            Err(CliError::ProfileNotFound)
        }
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
        let mut direct = HashSet::new();
        for (name, mut profile) in profiles {
            if let Some(shared) = credentials.get(name.as_str()) {
                profile.unsupported |= shared.unsupported;
                profile.options.extend(
                    shared
                        .options
                        .iter()
                        .map(|(key, value)| (key.clone(), *value)),
                );
            }
            if !profile.unsupported
                && !name.contains(['#', '\\'])
                && is_direct(&profile.options, &sessions)
            {
                direct.insert(name.clone());
            }
            known.insert(name);
        }
        known.extend(credentials.keys().map(|name| (*name).to_owned()));
        Ok(Self { known, direct })
    }
}

fn is_direct(options: &Options<'_>, sessions: &HashMap<String, Section<'_>>) -> bool {
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
        return false;
    }
    let Some(account) = options.get("sso_account_id") else {
        return false;
    };
    if account.len() != 12
        || !account.bytes().all(|b| b.is_ascii_digit())
        || !options
            .get("sso_role_name")
            .is_some_and(|role| !role.is_empty())
    {
        return false;
    }
    if options
        .get("sso_session")
        .is_some_and(|name| name.contains(['#', '\\']))
    {
        return false;
    }
    let settings = if let Some(name) = options.get("sso_session") {
        let Some(settings) = sessions.get(*name) else {
            return false;
        };
        if settings.unsupported {
            return false;
        }
        let settings = &settings.options;
        if ["sso_start_url", "sso_region"].into_iter().any(|key| {
            options
                .get(key)
                .is_some_and(|value| Some(value) != settings.get(key))
        }) {
            return false;
        }
        settings
    } else {
        options
    };
    settings
        .get("sso_region")
        .is_some_and(|region| valid_region(region))
        && settings.get("sso_start_url").is_some_and(|url| {
            url.strip_prefix("https://")
                .is_some_and(|suffix| !suffix.is_empty())
                && !url.chars().any(char::is_whitespace)
        })
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
