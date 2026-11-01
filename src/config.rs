//! Config schema v1 (spec §5): parsing, defaults, template, paths.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use url::Url;

use crate::domain::{RegistryId, RepoPath};

pub const TEMPLATE: &str = r#"# lcquay configuration. Path: ~/.config/lcquay/config.toml
schema_version = 1

# [[registries]]
# id = "company"
# provider = { type = "ecr", account_id = "123456789012", region = "us-east-1" }   # optional: profile = "work"
# # endpoint defaults to https://123456789012.dkr.ecr.us-east-1.amazonaws.com
# # auth defaults to { type = "ecr" }

# [[registries]]
# id = "company-public"
# provider = { type = "ecr-public", alias = "myalias" }
# # endpoint defaults to https://public.ecr.aws; repo paths are <alias>/<repo>

[[registries]]
id = "local"
provider = { type = "oci" }
endpoint = "http://localhost:5000"
repositories = ["team/api"]          # used when _catalog is unavailable; merged otherwise
# pinned = ["team/api"]              # shown first (★), their image counts load in the background
auth = { type = "anonymous" }

[[registries]]
id = "harbor"
provider = { type = "oci" }
endpoint = "https://harbor.example.com"
ref_host = "harbor.example.com"       # host used in image references; defaults to endpoint host
trusted_realms = ["auth.example.com"] # extra Bearer realm hosts allowed to receive credentials
auth = { type = "docker-config" }     # or { type = "basic", username = "ci", password_env = "HARBOR_PW" }
"#;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{}: {msg}", path.display())]
    File { path: PathBuf, msg: String },
    #[error("{}: registry {id:?}: {msg}", path.display())]
    Registry {
        path: PathBuf,
        id: String,
        msg: String,
    },
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

#[derive(Clone, Debug)]
pub struct Config {
    pub schema_version: u32,
    pub registries: Vec<RegistryCfg>,
}

/// Colour palette of the UI, picked in the app (`t`) and kept in `theme_path()`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Theme {
    TokyoNight,
    /// Tokyo Night accents on the terminal's own background.
    TokyoNightBlack,
    #[default]
    MapleDark,
    /// Maple Dark accents on the terminal's own background.
    MapleDarkBlack,
}

impl Theme {
    /// Picker order.
    pub const ALL: [Theme; 4] = [
        Theme::MapleDark,
        Theme::MapleDarkBlack,
        Theme::TokyoNight,
        Theme::TokyoNightBlack,
    ];

    /// The name stored in the theme file.
    pub fn name(self) -> &'static str {
        match self {
            Theme::TokyoNight => "tokyo-night",
            Theme::TokyoNightBlack => "tokyo-night-black",
            Theme::MapleDark => "maple-dark",
            Theme::MapleDarkBlack => "maple-dark-black",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Theme::TokyoNight => "Tokyo Night",
            Theme::TokyoNightBlack => "Tokyo Night Black (terminal background)",
            Theme::MapleDark => "Maple Dark",
            Theme::MapleDarkBlack => "Maple Dark Black (terminal background)",
        }
    }
}

/// The saved theme; a missing or unreadable file gives the default.
pub fn load_theme(path: &Path) -> Theme {
    let saved = std::fs::read_to_string(path).unwrap_or_default();
    Theme::ALL
        .into_iter()
        .find(|t| t.name() == saved.trim())
        .unwrap_or_default()
}

/// Writes `theme` to `path` (temp file + rename, so a crash never leaves a half-written file).
pub fn save_theme(path: &Path, theme: Theme) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    write_atomic(path, &format!("{}\n", theme.name()))
}

/// Replaces `path` with `text` through `<path>.tmp` + rename, so a crash never leaves a
/// half-written file. A symlinked file (a dotfiles repo) is resolved first: the link stays and
/// its target is replaced. The file keeps its permissions.
pub fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    let real = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
    let mut tmp = real.clone().into_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, text)?;
    let finish = || {
        if let Ok(meta) = std::fs::metadata(&real) {
            std::fs::set_permissions(&tmp, meta.permissions())?;
        }
        std::fs::rename(&tmp, &real)
    };
    finish().inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

#[derive(Clone, Debug, PartialEq)]
pub struct RegistryCfg {
    pub id: RegistryId,
    pub provider: ProviderCfg,
    pub endpoint: Url,
    pub ref_host: String,
    pub repositories: Vec<RepoPath>,
    pub pinned: Vec<RepoPath>,
    pub trusted_realms: Vec<String>,
    pub auth: AuthCfg,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderCfg {
    Ecr {
        account_id: String,
        region: String,
        profile: Option<String>,
    },
    EcrPublic {
        alias: String,
        profile: Option<String>,
    },
    Oci,
}

impl ProviderCfg {
    /// The `type` name used in the config file.
    pub fn kind(&self) -> &'static str {
        match self {
            ProviderCfg::Ecr { .. } => "ecr",
            ProviderCfg::EcrPublic { .. } => "ecr-public",
            ProviderCfg::Oci => "oci",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthCfg {
    Ecr,
    DockerConfig,
    Anonymous,
    /// Fixed username; the password is read from `$password_env` on every request.
    Basic {
        username: String,
        password_env: String,
    },
}

// Raw mirrors: serde ignores extra keys on unit variants of tagged enums even with
// `deny_unknown_fields`, so unit variants are written as empty struct variants here.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
enum RawProvider {
    Ecr {
        account_id: String,
        region: String,
        #[serde(default)]
        profile: Option<String>,
    },
    EcrPublic {
        alias: String,
        #[serde(default)]
        profile: Option<String>,
    },
    Oci {},
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
enum RawAuth {
    Ecr {},
    DockerConfig {},
    Anonymous {},
    Basic {
        username: String,
        password_env: String,
    },
}

impl From<RawProvider> for ProviderCfg {
    fn from(p: RawProvider) -> Self {
        match p {
            RawProvider::Ecr {
                account_id,
                region,
                profile,
            } => ProviderCfg::Ecr {
                account_id,
                region,
                profile,
            },
            RawProvider::EcrPublic { alias, profile } => ProviderCfg::EcrPublic { alias, profile },
            RawProvider::Oci {} => ProviderCfg::Oci,
        }
    }
}

impl From<RawAuth> for AuthCfg {
    fn from(a: RawAuth) -> Self {
        match a {
            RawAuth::Ecr {} => AuthCfg::Ecr,
            RawAuth::DockerConfig {} => AuthCfg::DockerConfig,
            RawAuth::Anonymous {} => AuthCfg::Anonymous,
            RawAuth::Basic {
                username,
                password_env,
            } => AuthCfg::Basic {
                username,
                password_env,
            },
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    schema_version: u32,
    /// Removed key, kept only to say where the setting went.
    theme: Option<toml::Value>,
    registries: Vec<toml::Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRegistry {
    id: String,
    provider: RawProvider,
    endpoint: Option<Url>,
    ref_host: Option<String>,
    #[serde(default)]
    repositories: Vec<String>,
    #[serde(default)]
    pinned: Vec<String>,
    #[serde(default)]
    trusted_realms: Vec<String>,
    auth: Option<RawAuth>,
}

pub fn parse(text: &str, path: &Path) -> Result<Config, ConfigError> {
    let file_err = |msg: String| ConfigError::File {
        path: path.to_owned(),
        msg,
    };
    let raw: RawConfig = toml::from_str(text).map_err(|e| file_err(e.to_string()))?;
    if raw.schema_version != 1 {
        return Err(file_err(format!(
            "unsupported schema_version {} (expected 1)",
            raw.schema_version
        )));
    }
    if raw.theme.is_some() {
        return Err(file_err(
            "`theme` is no longer a config key: remove it and press t in the app to pick a theme"
                .into(),
        ));
    }
    let mut registries: Vec<RegistryCfg> = Vec::new();
    for v in raw.registries {
        // Read the id first so every later error can name the registry.
        let id = v
            .get("id")
            .and_then(|i| i.as_str())
            .unwrap_or("?")
            .to_owned();
        let err = |msg: String| ConfigError::Registry {
            path: path.to_owned(),
            id: id.clone(),
            msg,
        };
        let r: RawRegistry = v.try_into().map_err(|e| err(e.to_string()))?;
        if registries.iter().any(|x| x.id.0 == r.id) {
            return Err(err("duplicate id".into()));
        }
        registries.push(resolve(r).map_err(err)?);
    }
    Ok(Config {
        schema_version: raw.schema_version,
        registries,
    })
}

fn resolve(r: RawRegistry) -> Result<RegistryCfg, String> {
    let provider = ProviderCfg::from(r.provider);
    let auth = r.auth.map(AuthCfg::from);
    if r.id.is_empty() {
        return Err("field `id` must not be empty".into());
    }
    let (endpoint, auth) = match &provider {
        ProviderCfg::Ecr { .. } | ProviderCfg::EcrPublic { .. } => {
            let auth = auth.unwrap_or(AuthCfg::Ecr);
            if auth != AuthCfg::Ecr {
                return Err(format!(
                    "field `auth`: provider {} requires {{ type = \"ecr\" }}",
                    provider.kind()
                ));
            }
            let default = match &provider {
                ProviderCfg::EcrPublic { alias, .. } => {
                    if alias.is_empty() {
                        return Err("field `alias` must not be empty".into());
                    }
                    "https://public.ecr.aws".to_string()
                }
                ProviderCfg::Ecr {
                    account_id, region, ..
                } => format!("https://{account_id}.dkr.ecr.{region}.amazonaws.com"),
                ProviderCfg::Oci => unreachable!("matched above"),
            };
            let endpoint = match r.endpoint {
                Some(e) => e,
                None => Url::parse(&default)
                    .map_err(|e| format!("fields `account_id`/`region`: {e}"))?,
            };
            (endpoint, auth)
        }
        ProviderCfg::Oci => {
            let auth = auth.ok_or("field `auth` is required for provider oci")?;
            if auth == AuthCfg::Ecr {
                return Err(
                    "field `auth`: auth type \"ecr\" is only valid for provider \"ecr\"".into(),
                );
            }
            if let AuthCfg::Basic {
                username,
                password_env,
            } = &auth
            {
                if username.is_empty() {
                    return Err("field `auth.username` must not be empty".into());
                }
                if password_env.is_empty() {
                    return Err("field `auth.password_env` must not be empty".into());
                }
            }
            (
                r.endpoint
                    .ok_or("field `endpoint` is required for provider oci")?,
                auth,
            )
        }
    };
    if !matches!(endpoint.scheme(), "http" | "https") {
        return Err("field `endpoint`: scheme must be http or https".into());
    }
    if !endpoint.username().is_empty() || endpoint.password().is_some() {
        return Err("field `endpoint`: must not contain credentials (user:pass@)".into());
    }
    if r.pinned.iter().any(String::is_empty) {
        return Err("field `pinned`: entries must not be empty".into());
    }
    let host = endpoint
        .host_str()
        .ok_or("field `endpoint`: missing host")?;
    let ref_host = r.ref_host.unwrap_or_else(|| match endpoint.port() {
        Some(p) => format!("{host}:{p}"),
        None => host.to_owned(),
    });
    Ok(RegistryCfg {
        id: RegistryId(r.id),
        provider,
        endpoint,
        ref_host,
        repositories: r.repositories.into_iter().map(RepoPath).collect(),
        pinned: r.pinned.into_iter().map(RepoPath).collect(),
        trusted_realms: r.trusted_realms,
        auth,
    })
}

/// Returns `Ok(None)` after writing the template when the file is missing.
pub fn load_or_write_template(path: &Path) -> Result<Option<Config>, ConfigError> {
    let io = |source| ConfigError::Io {
        path: path.to_owned(),
        source,
    };
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text, path).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).map_err(io)?;
            }
            std::fs::write(path, TEMPLATE).map_err(io)?;
            Ok(None)
        }
        Err(e) => Err(io(e)),
    }
}

/// `$VAR` if set to an absolute path (XDG rule), else `~/<default>`; same on every OS.
fn xdg_from(env: Option<OsString>, default: &str, file: &str) -> PathBuf {
    let base = env
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| {
            directories::BaseDirs::new()
                .map(|d| d.home_dir().to_owned())
                .unwrap_or_default()
                .join(default)
        });
    base.join("lcquay").join(file)
}

pub fn config_path() -> PathBuf {
    xdg_from(
        std::env::var_os("XDG_CONFIG_HOME"),
        ".config",
        "config.toml",
    )
}

pub fn theme_path() -> PathBuf {
    xdg_from(std::env::var_os("XDG_STATE_HOME"), ".local/state", "theme")
}

pub fn log_path() -> PathBuf {
    xdg_from(
        std::env::var_os("XDG_STATE_HOME"),
        ".local/state",
        "lcquay.log",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::path::Path;

    #[test]
    fn ecr_defaults() {
        let c = parse(
            r#"schema_version = 1
[[registries]]
id = "company"
provider = { type = "ecr", account_id = "123456789012", region = "us-west-2" }"#,
            Path::new("c.toml"),
        )
        .unwrap();
        let r = &c.registries[0];
        assert_eq!(
            r.endpoint.as_str(),
            "https://123456789012.dkr.ecr.us-west-2.amazonaws.com/"
        );
        assert_eq!(r.ref_host, "123456789012.dkr.ecr.us-west-2.amazonaws.com");
        assert_eq!(r.auth, AuthCfg::Ecr);
    }

    #[test]
    fn oci_local_with_port() {
        let c = parse(
            r#"schema_version = 1
[[registries]]
id = "local"
provider = { type = "oci" }
endpoint = "http://localhost:5000"
repositories = ["team/api"]
auth = { type = "anonymous" }"#,
            Path::new("c.toml"),
        )
        .unwrap();
        assert_eq!(c.registries[0].ref_host, "localhost:5000");
    }

    #[test]
    fn errors_name_id_and_field() {
        let e = parse(
            r#"schema_version = 1
[[registries]]
id = "local"
provider = { type = "oci" }
endpoint = "http://x"
auth = { type = "anonymous" }
reposit = []"#,
            Path::new("c.toml"),
        )
        .unwrap_err()
        .to_string();
        assert!(
            e.contains("c.toml") && e.contains("\"local\"") && e.contains("reposit"),
            "{e}"
        );
        let e = parse("schema_version = 2\nregistries = []", Path::new("c.toml"))
            .unwrap_err()
            .to_string();
        assert!(e.contains("c.toml") && e.contains("schema_version"), "{e}");
        let dup = r#"schema_version = 1
[[registries]]
id = "a"
provider = { type = "oci" }
endpoint = "http://x"
auth = { type = "anonymous" }
[[registries]]
id = "a"
provider = { type = "oci" }
endpoint = "http://y"
auth = { type = "anonymous" }"#;
        assert!(
            parse(dup, Path::new("c.toml"))
                .unwrap_err()
                .to_string()
                .contains("\"a\"")
        );
    }

    fn reg(body: &str) -> String {
        parse(
            &format!("schema_version = 1\n[[registries]]\nid = \"a\"\n{body}"),
            Path::new("c.toml"),
        )
        .unwrap_err()
        .to_string()
    }

    #[test]
    fn endpoint_with_userinfo_rejected() {
        let e = reg(
            "provider = { type = \"oci\" }\nendpoint = \"https://u:p@x\"\nauth = { type = \"anonymous\" }",
        );
        assert!(
            e.contains("\"a\"") && e.contains("endpoint") && !e.contains(":p@"),
            "{e}"
        );
    }

    #[test]
    fn invalid_registries() {
        let e = reg("provider = { type = \"oci\" }\nauth = { type = \"anonymous\" }");
        assert!(e.contains("\"a\"") && e.contains("endpoint"), "{e}");
        let e = reg("provider = { type = \"oci\" }\nendpoint = \"http://x\"");
        assert!(e.contains("\"a\"") && e.contains("auth"), "{e}");
        let e = reg(
            "provider = { type = \"ecr\", account_id = \"1\", region = \"r\" }\nauth = { type = \"anonymous\" }",
        );
        assert!(e.contains("\"a\"") && e.contains("auth"), "{e}");
        let e = reg(
            "provider = { type = \"oci\" }\nendpoint = \"http://x\"\nauth = { type = \"ecr\" }",
        );
        assert!(
            e.contains("\"a\"") && e.contains("auth") && e.contains("only valid"),
            "{e}"
        );
        let e = reg("provider = { type = \"oci\", bogus = 1 }");
        assert!(e.contains("\"a\"") && e.contains("bogus"), "{e}");
        let e = reg("provider = { type = \"ecr-public\" }");
        assert!(e.contains("\"a\"") && e.contains("alias"), "{e}");
        let e = reg(
            "provider = { type = \"oci\" }\nendpoint = \"http://x\"\nauth = { type = \"basic\" }",
        );
        assert!(e.contains("\"a\"") && e.contains("username"), "{e}");
        let e = reg(
            "provider = { type = \"oci\" }\nendpoint = \"http://x\"\nauth = { type = \"anonymous\", extra = 1 }",
        );
        assert!(e.contains("\"a\"") && e.contains("extra"), "{e}");
    }

    fn oci(auth: &str) -> String {
        format!(
            "schema_version = 1\n[[registries]]\nid = \"r\"\nprovider = {{ type = \"oci\" }}\n\
             endpoint = \"https://h.example\"\nauth = {auth}"
        )
    }

    #[test]
    fn basic_auth_parses() {
        let c = parse(
            &oci("{ type = \"basic\", username = \"ci\", password_env = \"HARBOR_PW\" }"),
            Path::new("c.toml"),
        )
        .unwrap();
        assert_eq!(
            c.registries[0].auth,
            AuthCfg::Basic {
                username: "ci".into(),
                password_env: "HARBOR_PW".into()
            }
        );
    }

    #[test]
    fn basic_auth_rejects_empty_fields() {
        for a in [
            "{ type = \"basic\", username = \"\", password_env = \"X\" }",
            "{ type = \"basic\", username = \"u\", password_env = \"\" }",
        ] {
            let e = parse(&oci(a), Path::new("c.toml")).unwrap_err().to_string();
            assert!(
                e.contains("registry \"r\"") && e.contains("must not be empty"),
                "{e}"
            );
        }
    }

    #[test]
    fn basic_auth_missing_password_env_is_error() {
        let e = parse(
            &oci("{ type = \"basic\", username = \"u\" }"),
            Path::new("c.toml"),
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("password_env"), "{e}");
    }

    #[test]
    fn basic_auth_rejects_inline_password() {
        let e = parse(
            &oci("{ type = \"basic\", username = \"u\", password_env = \"X\", password = \"x\" }"),
            Path::new("c.toml"),
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("password") && e.contains("unknown field"), "{e}");
    }

    #[test]
    fn basic_auth_only_for_oci() {
        let e = reg(
            "provider = { type = \"ecr\", account_id = \"1\", region = \"r\" }\nauth = { type = \"basic\", username = \"u\", password_env = \"X\" }",
        );
        assert!(e.contains("requires { type = \"ecr\" }"), "{e}");
    }

    fn ok(body: &str) -> RegistryCfg {
        parse(
            &format!("schema_version = 1\n[[registries]]\nid = \"a\"\n{body}"),
            Path::new("c.toml"),
        )
        .unwrap()
        .registries
        .remove(0)
    }

    #[test]
    fn ecr_public_defaults() {
        let r = ok("provider = { type = \"ecr-public\", alias = \"a1b2c3d4\" }");
        assert_eq!(r.endpoint.as_str(), "https://public.ecr.aws/");
        assert_eq!(r.ref_host, "public.ecr.aws");
        assert_eq!(r.auth, AuthCfg::Ecr);
        assert_eq!(
            r.provider,
            ProviderCfg::EcrPublic {
                alias: "a1b2c3d4".into(),
                profile: None
            }
        );
    }

    #[test]
    fn ecr_public_rejects_non_ecr_auth() {
        let e = reg(
            "provider = { type = \"ecr-public\", alias = \"x\" }\nauth = { type = \"anonymous\" }",
        );
        assert!(
            e.contains("provider ecr-public requires { type = \"ecr\" }"),
            "{e}"
        );
    }

    #[test]
    fn ecr_public_rejects_empty_alias() {
        let e = reg("provider = { type = \"ecr-public\", alias = \"\" }");
        assert!(e.contains("field `alias` must not be empty"), "{e}");
    }

    #[test]
    fn provider_kind_names() {
        let ecr = ProviderCfg::Ecr {
            account_id: "1".into(),
            region: "r".into(),
            profile: None,
        };
        let public = ProviderCfg::EcrPublic {
            alias: "x".into(),
            profile: None,
        };
        assert_eq!(
            [ecr.kind(), public.kind(), ProviderCfg::Oci.kind()],
            ["ecr", "ecr-public", "oci"]
        );
    }

    #[test]
    fn template_parses() {
        let c = parse(TEMPLATE, Path::new("t.toml")).unwrap();
        let ids: Vec<_> = c.registries.iter().map(|r| r.id.0.as_str()).collect();
        assert_eq!(ids, ["local", "harbor"]);
        assert_eq!(c.registries[1].trusted_realms, ["auth.example.com"]);
        assert_eq!(c.registries[1].auth, AuthCfg::DockerConfig);
    }

    #[test]
    fn theme_key_is_rejected_with_a_pointer() {
        let p = Path::new("t.toml");
        let text = "schema_version = 1\ntheme = \"maple-dark\"\nregistries = []\n";
        let e = parse(text, p).unwrap_err().to_string();
        assert!(e.contains("press t"), "{e}");
    }

    #[test]
    fn theme_file_round_trip_and_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lcquay/theme");
        assert_eq!(load_theme(&path), Theme::MapleDark, "missing file");
        for t in Theme::ALL {
            save_theme(&path, t).unwrap();
            assert_eq!(load_theme(&path), t);
        }
        std::fs::write(&path, "solarized\n").unwrap();
        assert_eq!(load_theme(&path), Theme::MapleDark, "unknown name");
    }

    #[test]
    fn missing_file_writes_template() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("lcquay/config.toml");
        assert!(load_or_write_template(&p).unwrap().is_none());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), TEMPLATE);
        assert!(load_or_write_template(&p).unwrap().is_some());
    }

    #[test]
    fn xdg_paths() {
        let abs = std::env::temp_dir();
        assert_eq!(
            xdg_from(Some(abs.clone().into()), ".config", "config.toml"),
            abs.join("lcquay/config.toml")
        );
        // relative or unset falls back to ~/<default>
        for env in [None, Some(OsString::from("rel"))] {
            let p = xdg_from(env, ".local/state", "lcquay.log");
            assert!(p.ends_with(".local/state/lcquay/lcquay.log"), "{p:?}");
        }
    }

    #[test]
    fn pinned_parses_and_defaults() {
        let p = |r: &RegistryCfg| r.pinned.iter().map(|p| p.0.clone()).collect::<Vec<_>>();
        let oci = "provider = { type = \"oci\" }\nendpoint = \"http://x\"\nauth = { type = \"anonymous\" }\n";
        assert!(ok(oci).pinned.is_empty());
        let r = ok(&format!("{oci}pinned = [\"api-svc\", \"eng/api/fo\"]"));
        assert_eq!(p(&r), ["api-svc", "eng/api/fo"]);
        let e = reg(&format!("{oci}pinned = [\"\"]"));
        assert!(
            e.contains("field `pinned`: entries must not be empty"),
            "{e}"
        );
    }
}
