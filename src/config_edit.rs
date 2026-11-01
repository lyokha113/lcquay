//! Comment- and order-preserving edits of `config.toml` registries (`toml_edit`), the form
//! model the registry form (Task 11) edits, and its validation.

use std::path::Path;

use toml_edit::{Array, ArrayOfTables, DocumentMut, InlineTable, Item, Table, TableLike, Value};

use crate::config::{self, ConfigError, RegistryCfg};
use crate::domain::RegistryId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Ecr,
    EcrPublic,
    Oci,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthKind {
    Anonymous,
    DockerConfig,
    Basic,
}

/// Every field as the user types it; list fields are comma-separated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegistryForm {
    pub id: String,
    pub kind: Kind,
    pub pinned: String,
    pub account_id: String,
    pub region: String,
    pub profile: String,
    pub alias: String,
    pub endpoint: String,
    pub ref_host: String,
    pub auth: AuthKind,
    pub username: String,
    pub password_env: String,
    pub repositories: String,
    pub trusted_realms: String,
}

impl RegistryForm {
    pub fn new_default() -> Self {
        RegistryForm {
            id: String::new(),
            kind: Kind::Oci,
            pinned: String::new(),
            account_id: String::new(),
            region: String::new(),
            profile: String::new(),
            alias: String::new(),
            endpoint: String::new(),
            ref_host: String::new(),
            auth: AuthKind::Anonymous,
            username: String::new(),
            password_env: String::new(),
            repositories: String::new(),
            trusted_realms: String::new(),
        }
    }
}

const NOT_FOUND: &str = "registry not found in the config file";

fn parse_doc(text: &str) -> Result<DocumentMut, String> {
    text.parse()
        .map_err(|e: toml_edit::TomlError| e.to_string())
}

fn find(doc: &DocumentMut, id: &str) -> Option<usize> {
    let regs = doc.get("registries")?.as_array_of_tables()?;
    regs.iter()
        .position(|t| t.get("id").and_then(Item::as_str) == Some(id))
}

fn str_of(t: &dyn TableLike, key: &str) -> String {
    t.get(key)
        .and_then(Item::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn list_of(t: &dyn TableLike, key: &str) -> Vec<String> {
    let strs = t.get(key).and_then(Item::as_array);
    strs.into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect()
}

fn split_list(s: &str) -> Vec<String> {
    s.split(',')
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The registry's raw file values; overrides stay empty when the key is absent.
pub fn read_form(text: &str, id: &RegistryId) -> Result<RegistryForm, String> {
    let doc = parse_doc(text)?;
    let at = find(&doc, &id.0).ok_or(NOT_FOUND)?;
    let regs = doc["registries"].as_array_of_tables();
    let t = regs.and_then(|r| r.get(at)).expect("found above");
    let mut f = RegistryForm::new_default();
    f.id = id.0.clone();
    f.endpoint = str_of(t, "endpoint");
    f.ref_host = str_of(t, "ref_host");
    f.pinned = list_of(t, "pinned").join(", ");
    f.repositories = list_of(t, "repositories").join(", ");
    f.trusted_realms = list_of(t, "trusted_realms").join(", ");
    let provider = t
        .get("provider")
        .and_then(Item::as_table_like)
        .ok_or("field `provider` is missing")?;
    f.kind = match str_of(provider, "type").as_str() {
        "ecr" => Kind::Ecr,
        "ecr-public" => Kind::EcrPublic,
        "oci" => Kind::Oci,
        other => return Err(format!("unknown provider type {other:?}")),
    };
    f.account_id = str_of(provider, "account_id");
    f.region = str_of(provider, "region");
    f.alias = str_of(provider, "alias");
    f.profile = str_of(provider, "profile");
    if let Some(auth) = t.get("auth").and_then(Item::as_table_like) {
        f.auth = match str_of(auth, "type").as_str() {
            "docker-config" => AuthKind::DockerConfig,
            "basic" => AuthKind::Basic,
            _ => AuthKind::Anonymous,
        };
        f.username = str_of(auth, "username");
        f.password_env = str_of(auth, "password_env");
    }
    Ok(f)
}

fn trimmed(f: &RegistryForm) -> RegistryForm {
    let t = |s: &String| s.trim().to_owned();
    RegistryForm {
        id: t(&f.id),
        pinned: t(&f.pinned),
        account_id: t(&f.account_id),
        region: t(&f.region),
        profile: t(&f.profile),
        alias: t(&f.alias),
        endpoint: t(&f.endpoint),
        ref_host: t(&f.ref_host),
        username: t(&f.username),
        password_env: t(&f.password_env),
        repositories: t(&f.repositories),
        trusted_realms: t(&f.trusted_realms),
        ..f.clone()
    }
}

/// Checks the form by writing it into an empty config and parsing that, so the rules are exactly
/// the ones the loader applies, plus the form-only 12-digit account rule.
pub fn validate(form: &RegistryForm, taken: &[RegistryId]) -> Result<RegistryCfg, String> {
    let f = trimmed(form);
    if f.id.is_empty() {
        return Err("field `id` must not be empty".into());
    }
    if taken.iter().any(|t| t.0 == f.id) {
        return Err("duplicate id".into());
    }
    if f.kind == Kind::Ecr {
        if f.account_id.len() != 12 || !f.account_id.bytes().all(|b| b.is_ascii_digit()) {
            return Err("field `account_id` must be 12 digits".into());
        }
        if f.region.is_empty() {
            return Err("field `region` must not be empty".into());
        }
    }
    let text = upsert("schema_version = 1\n", None, &f)?;
    let cfg = config::parse(&text, Path::new("")).map_err(|e| match e {
        ConfigError::Registry { msg, .. } | ConfigError::File { msg, .. } => msg,
        ConfigError::Io { source, .. } => source.to_string(),
    })?;
    cfg.registries
        .into_iter()
        .next()
        .ok_or_else(|| "no registry written".into())
}

/// Sets `key` to `v`, keeping the old value's surrounding whitespace and inline comment.
fn set_value(t: &mut dyn TableLike, key: &str, mut v: Value) {
    if let Some(old) = t.get(key).and_then(Item::as_value) {
        *v.decor_mut() = old.decor().clone();
    }
    t.insert(key, Item::Value(v));
}

fn set_str(t: &mut dyn TableLike, key: &str, val: &str) {
    if t.get(key).and_then(Item::as_str) != Some(val) {
        set_value(t, key, val.into());
    }
}

/// An empty value removes the key.
fn set_opt_str(t: &mut dyn TableLike, key: &str, val: &str) {
    if val.is_empty() {
        t.remove(key);
    } else {
        set_str(t, key, val);
    }
}

/// An empty list removes the key; an unchanged one is left byte for byte.
fn set_list(t: &mut dyn TableLike, key: &str, val: &str) {
    let list = split_list(val);
    if list.is_empty() {
        t.remove(key);
    } else if list_of(t, key) != list || t.get(key).and_then(Item::as_array).is_none() {
        set_value(
            t,
            key,
            Array::from_iter(list.iter().map(String::as_str)).into(),
        );
    }
}

/// Sets `type` and the given keys on `item`. A different `type` replaces the whole inline table,
/// so keys of the old type go; the same `type` is edited key by key.
fn set_typed(item: &mut Item, ty: &str, keys: &[(&str, &str, bool)]) {
    let same = item
        .as_table_like()
        .is_some_and(|t| str_of(t, "type") == ty);
    if !same {
        *item = Item::Value(InlineTable::new().into());
    }
    let Some(t) = item.as_table_like_mut() else {
        return;
    };
    set_str(t, "type", ty);
    for &(key, val, optional) in keys {
        if optional {
            set_opt_str(t, key, val);
        } else {
            set_str(t, key, val);
        }
    }
    if let Some(inline) = item.as_inline_table_mut() {
        inline.fmt();
    }
}

fn apply(t: &mut Table, f: &RegistryForm, was: Option<&str>) {
    set_str(t, "id", &f.id);
    let provider = t.entry("provider").or_insert(Item::None);
    match f.kind {
        Kind::Ecr => set_typed(
            provider,
            "ecr",
            &[
                ("account_id", &f.account_id, false),
                ("region", &f.region, false),
                ("profile", &f.profile, true),
            ],
        ),
        Kind::EcrPublic => set_typed(
            provider,
            "ecr-public",
            &[("alias", &f.alias, false), ("profile", &f.profile, true)],
        ),
        Kind::Oci => set_typed(provider, "oci", &[]),
    }
    set_opt_str(t, "endpoint", &f.endpoint);
    set_opt_str(t, "ref_host", &f.ref_host);
    if f.kind == Kind::Oci {
        set_list(t, "repositories", &f.repositories);
    }
    set_list(t, "pinned", &f.pinned);
    if f.kind != Kind::Oci {
        // Only a type change drops these, so hand-written ecr entries round-trip untouched.
        if was.is_none_or(|w| w == "oci") {
            for key in ["auth", "repositories", "trusted_realms"] {
                t.remove(key);
            }
        }
        return;
    }
    set_list(t, "trusted_realms", &f.trusted_realms);
    let auth = t.entry("auth").or_insert(Item::None);
    match f.auth {
        AuthKind::Anonymous => set_typed(auth, "anonymous", &[]),
        AuthKind::DockerConfig => set_typed(auth, "docker-config", &[]),
        AuthKind::Basic => set_typed(
            auth,
            "basic",
            &[
                ("username", &f.username, false),
                ("password_env", &f.password_env, false),
            ],
        ),
    }
}

/// Edits the registry `original` in place, or appends a new one when it is `None`.
pub fn upsert(
    text: &str,
    original: Option<&RegistryId>,
    form: &RegistryForm,
) -> Result<String, String> {
    let f = trimmed(form);
    let mut doc = parse_doc(text)?;
    let at = original
        .map(|o| find(&doc, &o.0).ok_or(NOT_FOUND))
        .transpose()?;
    if find(&doc, &f.id).is_some_and(|i| Some(i) != at) {
        return Err("duplicate id".into());
    }
    // A missing key or an empty inline `registries = []` becomes a real array of tables; a
    // non-empty inline array is left alone (converting it is not this writer's job).
    match doc.get("registries") {
        Some(i) if i.is_array_of_tables() => {}
        Some(i) if i.as_array().is_none_or(|a| !a.is_empty()) => {
            return Err("registries is an inline array; edit the file with `e`".into());
        }
        _ => {
            doc.remove("registries");
            doc.insert("registries", Item::ArrayOfTables(ArrayOfTables::new()));
        }
    }
    // A new header needs a blank line before it; toml_edit adds none.
    let before = doc.to_string();
    let gap = if before.is_empty() || before.ends_with("\n\n") {
        ""
    } else if before.ends_with('\n') {
        "\n"
    } else {
        "\n\n"
    };
    let regs = doc["registries"]
        .as_array_of_tables_mut()
        .expect("set above");
    match at {
        Some(i) => {
            let was = regs
                .get(i)
                .and_then(|t| t.get("provider"))
                .and_then(Item::as_table_like)
                .map(|p| str_of(p, "type"));
            apply(regs.get_mut(i).expect("found above"), &f, was.as_deref());
        }
        None => {
            let mut t = Table::new();
            t.decor_mut().set_prefix(gap);
            apply(&mut t, &f, None);
            regs.push(t);
        }
    }
    Ok(doc.to_string())
}

/// Drops the registry's table, with the comments directly above it.
pub fn remove(text: &str, id: &RegistryId) -> Result<String, String> {
    let mut doc = parse_doc(text)?;
    let at = find(&doc, &id.0).ok_or(NOT_FOUND)?;
    let regs = doc["registries"]
        .as_array_of_tables_mut()
        .expect("found above");
    regs.remove(at);
    if regs.is_empty() {
        // The loader requires the key.
        doc["registries"] = Item::Value(Array::new().into());
    }
    Ok(doc.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = r##"# lcquay config
schema_version = 1

[[registries]]
id = "local"
provider = { type = "oci" }
endpoint = "http://localhost:5000"
auth = { type = "anonymous" }

# the corporate Harbor
[[registries]]
id = "harbor"
provider = { type = "oci" }
endpoint = "https://harbor.example.com"   # behind the VPN
ref_host = "harbor.example.com"
repositories = ["team/api"]   # fallback list
trusted_realms = ["auth.example.com"]
auth = { type = "basic", username = "ci", password_env = "HARBOR_PW" }

[[registries]]
id = "prod"
provider = { type = "ecr", account_id = "123456789012", region = "us-east-1" }
"##;

    fn id(s: &str) -> RegistryId {
        RegistryId(s.into())
    }

    fn form(file: &str, name: &str) -> RegistryForm {
        read_form(file, &id(name)).unwrap()
    }

    fn parsed(text: &str) -> config::Config {
        config::parse(text, Path::new("t.toml")).unwrap()
    }

    #[test]
    fn round_trip_untouched() {
        for name in ["local", "harbor", "prod"] {
            let out = upsert(FILE, Some(&id(name)), &form(FILE, name)).unwrap();
            assert_eq!(out, FILE, "{name}");
        }
    }

    #[test]
    fn read_form_reads_raw_values() {
        let f = form(FILE, "harbor");
        assert_eq!(f.kind, Kind::Oci);
        assert_eq!(f.endpoint, "https://harbor.example.com");
        assert_eq!(f.ref_host, "harbor.example.com");
        assert_eq!(f.repositories, "team/api");
        assert_eq!(f.trusted_realms, "auth.example.com");
        assert_eq!(f.auth, AuthKind::Basic);
        assert_eq!(
            (f.username.as_str(), f.password_env.as_str()),
            ("ci", "HARBOR_PW")
        );
        let f = form(FILE, "prod");
        assert_eq!(f.kind, Kind::Ecr);
        assert_eq!(
            (f.account_id.as_str(), f.region.as_str()),
            ("123456789012", "us-east-1")
        );
        assert_eq!(f.endpoint, "", "no override in the file");
        assert!(read_form(FILE, &id("nope")).is_err());
    }

    #[test]
    fn edit_in_place_keeps_comments_order() {
        let mut f = form(FILE, "harbor");
        f.repositories = "a, b".into();
        let out = upsert(FILE, Some(&id("harbor")), &f).unwrap();
        let want = FILE.replace(
            r#"repositories = ["team/api"]   # fallback list"#,
            r#"repositories = ["a", "b"]   # fallback list"#,
        );
        assert_eq!(out, want);
        let ids: Vec<_> = parsed(&out)
            .registries
            .iter()
            .map(|r| r.id.0.clone())
            .collect();
        assert_eq!(ids, ["local", "harbor", "prod"]);
    }

    #[test]
    fn append_new_at_end() {
        let mut f = RegistryForm::new_default();
        f.id = "local2".into();
        f.endpoint = "http://localhost:5001".into();
        f.repositories = "x/y".into();
        let out = upsert(FILE, None, &f).unwrap();
        assert!(out.starts_with(FILE), "existing text is untouched");
        let cfg = parsed(&out);
        assert_eq!(cfg.registries.len(), 4);
        let last = cfg.registries.last().unwrap();
        assert_eq!(last.id.0, "local2");
        assert_eq!(last.repositories[0].0, "x/y");
        assert!(out.ends_with("\n"));
    }

    #[test]
    fn append_to_missing_or_empty_registries() {
        let mut f = RegistryForm::new_default();
        f.id = "a".into();
        f.endpoint = "http://a".into();
        for text in [
            "schema_version = 1\n",
            "schema_version = 1\nregistries = []\n",
        ] {
            let out = upsert(text, None, &f).unwrap();
            assert_eq!(parsed(&out).registries.len(), 1, "{out}");
        }
    }

    #[test]
    fn upsert_rejects_unknown_original_and_duplicates() {
        let f = form(FILE, "harbor");
        assert!(upsert(FILE, Some(&id("nope")), &f).is_err());
        assert!(upsert(FILE, None, &f).unwrap_err().contains("duplicate id"));
        let mut renamed = form(FILE, "harbor");
        renamed.id = "local".into();
        assert!(
            upsert(FILE, Some(&id("harbor")), &renamed)
                .unwrap_err()
                .contains("duplicate id")
        );
        renamed.id = "harbor2".into();
        let out = upsert(FILE, Some(&id("harbor")), &renamed).unwrap();
        assert!(out.contains("id = \"harbor2\""));
        assert!(out.contains("# the corporate Harbor"));
    }

    #[test]
    fn remove_table() {
        let out = remove(FILE, &id("harbor")).unwrap();
        let want = FILE.replace(
            &FILE[FILE.find("# the corporate").unwrap()
                ..FILE.find("[[registries]]\nid = \"prod\"").unwrap()],
            "",
        );
        assert_eq!(out, want);
        assert!(!out.contains("corporate"));
        let ids: Vec<_> = parsed(&out)
            .registries
            .iter()
            .map(|r| r.id.0.clone())
            .collect();
        assert_eq!(ids, ["local", "prod"]);
        assert!(remove(FILE, &id("nope")).is_err());
    }

    #[test]
    fn remove_last_registry_keeps_file_parseable() {
        let text = "schema_version = 1\n\n[[registries]]\nid = \"a\"\nprovider = { type = \"oci\" }\nendpoint = \"http://a\"\nauth = { type = \"anonymous\" }\n";
        let out = remove(text, &id("a")).unwrap();
        assert!(parsed(&out).registries.is_empty(), "{out}");
    }

    #[test]
    fn type_change_drops_old_keys() {
        let mut f = form(FILE, "harbor");
        f.kind = Kind::Ecr;
        f.account_id = "123456789012".into();
        f.region = "eu-west-1".into();
        f.endpoint = String::new();
        f.ref_host = String::new();
        let out = upsert(FILE, Some(&id("harbor")), &f).unwrap();
        let tail = &out[out.find("id = \"harbor\"").unwrap()..out.find("id = \"prod\"").unwrap()];
        assert_eq!(
            tail,
            "id = \"harbor\"\nprovider = { type = \"ecr\", account_id = \"123456789012\", region = \"eu-west-1\" }\n\n[[registries]]\n"
        );
        parsed(&out);

        let mut f = form(FILE, "prod");
        f.kind = Kind::EcrPublic;
        f.alias = "myalias".into();
        let out = upsert(FILE, Some(&id("prod")), &f).unwrap();
        assert!(
            out.ends_with("provider = { type = \"ecr-public\", alias = \"myalias\" }\n"),
            "{out}"
        );
        assert!(!out.contains("account_id = \"123456789012\""));

        // and back to oci: provider is just the type again
        let mut f = form(FILE, "prod");
        f.kind = Kind::Oci;
        f.endpoint = "http://x".into();
        let out = upsert(FILE, Some(&id("prod")), &f).unwrap();
        assert!(out.contains("provider = { type = \"oci\" }\nendpoint = \"http://x\"\nauth = { type = \"anonymous\" }\n"), "{out}");
        parsed(&out);
    }

    #[test]
    fn same_type_edits_keys_and_profile() {
        let mut f = form(FILE, "prod");
        f.region = "eu-west-1".into();
        f.profile = "work".into();
        let out = upsert(FILE, Some(&id("prod")), &f).unwrap();
        assert!(out.ends_with("provider = { type = \"ecr\", account_id = \"123456789012\", region = \"eu-west-1\", profile = \"work\" }\n"), "{out}");
        f.profile.clear();
        let out2 = upsert(&out, Some(&id("prod")), &f).unwrap();
        assert!(out2.ends_with("region = \"eu-west-1\" }\n"), "{out2}");
    }

    #[test]
    fn cleared_override_removes_key() {
        let mut f = form(FILE, "harbor");
        f.ref_host.clear();
        f.endpoint = "https://h2.example.com".into();
        let out = upsert(FILE, Some(&id("harbor")), &f).unwrap();
        assert!(!out.contains("ref_host"));
        assert!(
            out.contains("endpoint = \"https://h2.example.com\"   # behind the VPN"),
            "{out}"
        );
        f.repositories.clear();
        f.trusted_realms.clear();
        let out = upsert(&out, Some(&id("harbor")), &f).unwrap();
        assert!(!out.contains("repositories") && !out.contains("trusted_realms"));
    }

    #[test]
    fn lists_are_trimmed_and_empties_dropped() {
        let mut f = form(FILE, "harbor");
        f.pinned = " a ,, b/c ,".into();
        let out = upsert(FILE, Some(&id("harbor")), &f).unwrap();
        assert!(out.contains("pinned = [\"a\", \"b/c\"]\n"), "{out}");
    }

    #[test]
    fn basic_auth_never_has_secret() {
        let mut f = RegistryForm::new_default();
        f.id = "b".into();
        f.endpoint = "http://b".into();
        f.auth = AuthKind::Basic;
        f.username = "ci".into();
        f.password_env = "HARBOR_PW".into();
        let out = upsert("schema_version = 1\n", None, &f).unwrap();
        assert!(
            out.contains(
                "auth = { type = \"basic\", username = \"ci\", password_env = \"HARBOR_PW\" }\n"
            ),
            "{out}"
        );
        assert_eq!(
            out.matches("pass").count(),
            1,
            "only the env var name key: {out}"
        );
        // switching away from basic drops the keys
        f.auth = AuthKind::DockerConfig;
        let out = upsert(&out, Some(&id("b")), &f).unwrap();
        assert!(
            out.contains("auth = { type = \"docker-config\" }\n"),
            "{out}"
        );
    }

    #[test]
    fn validate_errors() {
        let mut f = RegistryForm::new_default();
        assert_eq!(
            validate(&f, &[]).unwrap_err(),
            "field `id` must not be empty"
        );
        f.id = "x".into();
        assert_eq!(validate(&f, &[id("x")]).unwrap_err(), "duplicate id");
        assert_eq!(
            validate(&f, &[]).unwrap_err(),
            "field `endpoint` is required for provider oci"
        );
        f.kind = Kind::Ecr;
        f.account_id = "12ab".into();
        f.region = "us-east-1".into();
        assert_eq!(
            validate(&f, &[]).unwrap_err(),
            "field `account_id` must be 12 digits"
        );
        f.account_id = "123456789012".into();
        f.region.clear();
        assert_eq!(
            validate(&f, &[]).unwrap_err(),
            "field `region` must not be empty"
        );
        f.region = "us-east-1".into();
        let cfg = validate(&f, &[]).unwrap();
        assert_eq!(
            cfg.endpoint.as_str(),
            "https://123456789012.dkr.ecr.us-east-1.amazonaws.com/"
        );
        f.kind = Kind::EcrPublic;
        f.alias.clear();
        assert_eq!(
            validate(&f, &[]).unwrap_err(),
            "field `alias` must not be empty"
        );
    }

    #[test]
    fn validate_trims_and_returns_cfg() {
        let mut f = RegistryForm::new_default();
        f.id = " reg ".into();
        f.endpoint = " http://localhost:5000 ".into();
        f.pinned = "a, b".into();
        let cfg = validate(&f, &[id("other")]).unwrap();
        assert_eq!(cfg.id.0, "reg");
        assert_eq!(cfg.pinned.len(), 2);
        assert_eq!(cfg.ref_host, "localhost:5000");
    }

    #[test]
    fn non_empty_inline_registries_is_never_clobbered() {
        let text = "schema_version = 1\nregistries = [ { id = \"a\", provider = { type = \"oci\" }, endpoint = \"http://a\", auth = { type = \"anonymous\" } } ]\n";
        parsed(text);
        let mut f = RegistryForm::new_default();
        f.id = "b".into();
        f.endpoint = "http://b".into();
        let e = upsert(text, None, &f).unwrap_err();
        assert!(e.contains("inline array"), "{e}");
        // edit and remove keep their existing errors
        assert_eq!(upsert(text, Some(&id("a")), &f).unwrap_err(), NOT_FOUND);
        assert_eq!(remove(text, &id("a")).unwrap_err(), NOT_FOUND);
    }

    #[test]
    fn write_atomic_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "old").unwrap();
        config::write_atomic(&p, "new").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "new");
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["config.toml"], "no .tmp left");
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_follows_symlink() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("dotfiles")).unwrap();
        let target = dir.path().join("dotfiles/config.toml");
        std::fs::write(&target, "old").unwrap();
        let link = dir.path().join("config.toml");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        config::write_atomic(&link, "new").unwrap();
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
        assert!(!dir.path().join("dotfiles/config.toml.tmp").exists());
    }
}
