//! Best-effort secret masking (spec §6.3). Pure functions, no I/O.
use std::collections::BTreeMap;

pub const MASK: &str = "••••••";

const PATTERNS: [&str; 6] = ["PASS", "SECRET", "TOKEN", "KEY", "CRED", "AUTH"];

pub fn is_secret_key(k: &str) -> bool {
    let k = k.to_uppercase();
    PATTERNS.iter().any(|p| k.contains(p))
}

fn mask_value<'a>(k: &str, v: &'a str) -> &'a str {
    if is_secret_key(k) { MASK } else { v }
}

pub fn mask_pairs(pairs: &[(String, String)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(k, v)| (k.clone(), mask_value(k, v).to_string()))
        .collect()
}

pub fn mask_map(m: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    m.iter()
        .map(|(k, v)| (k.clone(), mask_value(k, v).to_string()))
        .collect()
}

/// Byte length of the value at the start of `s`: up to whitespace, or, when it starts
/// with a quote, through the closing quote (spaces inside included) and any glued suffix.
fn value_len(s: &str) -> usize {
    let end = |from: usize| {
        s[from..]
            .find(char::is_whitespace)
            .map_or(s.len(), |i| from + i)
    };
    match s.chars().next() {
        Some(q @ ('"' | '\'')) => s[1..].find(q).map_or(s.len(), |i| end(i + 2)),
        _ => end(0),
    }
}

/// Masks `KEY=value`, `--key=value` and `--key value` with secret keys in a command line.
/// Whitespace and all other tokens are preserved. Quoted values are masked through the
/// closing quote. Best effort (no shell parsing).
pub fn mask_command(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    let mut pending = false; // previous token was a secret `--flag` awaiting its value
    while !rest.is_empty() {
        let ws = rest.len() - rest.trim_start().len();
        out.push_str(&rest[..ws]);
        rest = &rest[ws..];
        if rest.is_empty() {
            break;
        }
        let tok_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let tok = &rest[..tok_end];
        if pending && !tok.starts_with('-') {
            out.push_str(MASK);
            rest = &rest[value_len(rest)..];
            pending = false;
            continue;
        }
        pending = false;
        let name = |k: &str| k.trim_start_matches('-').to_string();
        match tok.find('=') {
            Some(i) if is_secret_key(&name(&tok[..i])) && value_len(&rest[i + 1..]) > 0 => {
                out.push_str(&rest[..=i]);
                out.push_str(MASK);
                rest = &rest[i + 1 + value_len(&rest[i + 1..])..];
            }
            None if tok.starts_with('-') && is_secret_key(&name(tok)) => {
                pending = true;
                out.push_str(tok);
                rest = &rest[tok_end..];
            }
            _ => {
                out.push_str(tok);
                rest = &rest[tok_end..];
            }
        }
    }
    out
}

fn mask_str_values(v: &mut serde_json::Value) {
    if let Some(o) = v.as_object_mut() {
        for (k, val) in o {
            if is_secret_key(k) && val.is_string() {
                *val = MASK.into();
            }
        }
    }
}

/// Masks `Env`, `Labels`, `Entrypoint` and `Cmd` under `base` (`/config` or the legacy `/container_config`).
fn mask_container(v: &mut serde_json::Value, base: &str) {
    if let Some(env) = v
        .pointer_mut(&format!("{base}/Env"))
        .and_then(|e| e.as_array_mut())
    {
        for e in env {
            if let Some((k, _)) = e.as_str().and_then(|s| s.split_once('='))
                && is_secret_key(k)
            {
                *e = format!("{k}={MASK}").into();
            }
        }
    }
    if let Some(l) = v.pointer_mut(&format!("{base}/Labels")) {
        mask_str_values(l);
    }
    for field in ["Entrypoint", "Cmd"] {
        let Some(arr) = v
            .pointer_mut(&format!("{base}/{field}"))
            .and_then(|c| c.as_array_mut())
        else {
            continue;
        };
        let mut pending = false; // previous element was a secret `--flag` awaiting its value
        for c in arr {
            let Some(s) = c.as_str() else { continue };
            if pending && !s.starts_with('-') {
                *c = MASK.into();
                pending = false;
                continue;
            }
            pending =
                s.starts_with('-') && !s.contains('=') && is_secret_key(s.trim_start_matches('-'));
            *c = mask_command(s).into();
        }
    }
}

/// Masks `Env`/`Labels`/`Cmd` of `config` and `container_config`, `history[].created_by`, and
/// `annotations` (top level, `config`/`subject` descriptors, every `layers[]` and `manifests[]`
/// entry), then pretty-prints. Invalid JSON is returned unchanged.
pub fn mask_config_json(raw: &str) -> String {
    let Ok(mut v) = serde_json::from_str::<serde_json::Value>(raw) else {
        return raw.to_string();
    };
    mask_container(&mut v, "/config");
    mask_container(&mut v, "/container_config");
    for p in [
        "/annotations",
        "/config/annotations",
        "/subject/annotations",
    ] {
        if let Some(a) = v.pointer_mut(p) {
            mask_str_values(a);
        }
    }
    for list in ["layers", "manifests"] {
        for d in v
            .get_mut(list)
            .and_then(|l| l.as_array_mut())
            .into_iter()
            .flatten()
        {
            if let Some(a) = d.get_mut("annotations") {
                mask_str_values(a);
            }
        }
    }
    for step in v
        .get_mut("history")
        .and_then(|h| h.as_array_mut())
        .into_iter()
        .flatten()
    {
        if let Some(c) = step.get_mut("created_by")
            && let Some(s) = c.as_str()
        {
            *c = mask_command(s).into();
        }
    }
    serde_json::to_string_pretty(&v).unwrap_or_else(|_| raw.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entrypoint_and_cmd_flag_value_pairs_masked() {
        let raw = r#"{"config":{"Entrypoint":["app","--password","x1"],"Cmd":["--api-key","x2","--port","80"]},
            "container_config":{"Cmd":["--token","x3"]}}"#;
        let out = mask_config_json(raw);
        for s in ["x1", "x2", "x3"] {
            assert!(!out.contains(s), "{s} leaked: {out}");
        }
        assert!(out.contains("\"80\"") && out.contains("--port"), "{out}");
    }

    #[test]
    fn keys() {
        assert!(is_secret_key("DB_PASSWORD"));
        assert!(is_secret_key("x-auth"));
        assert!(!is_secret_key("PATH"));
    }

    #[test]
    fn command_forms() {
        assert_eq!(
            mask_command("RUN API_TOKEN=abc ./x --password=hunter2 --secret s3 --name n"),
            "RUN API_TOKEN=•••••• ./x --password=•••••• --secret •••••• --name n"
        );
    }

    #[test]
    fn command_preserves_spacing_and_quotes() {
        assert_eq!(
            mask_command("a  TOKEN=\"x y\"   b=\"c d\"\tPASS='p q' z"),
            "a  TOKEN=••••••   b=\"c d\"\tPASS=•••••• z"
        );
        // secret flag followed by another flag: nothing to mask
        assert_eq!(mask_command("--token --name n"), "--token --name n");
        assert_eq!(mask_command("TOKEN= x"), "TOKEN= x");
    }

    #[test]
    fn pairs_and_map() {
        let p = vec![
            ("A".to_string(), "1".to_string()),
            ("MY_KEY".to_string(), "v".to_string()),
        ];
        assert_eq!(mask_pairs(&p)[0].1, "1");
        assert_eq!(mask_pairs(&p)[1].1, MASK);
        let m: BTreeMap<_, _> = p.into_iter().collect();
        assert_eq!(mask_map(&m)["MY_KEY"], MASK);
        assert_eq!(mask_map(&m)["A"], "1");
    }

    #[test]
    fn config_json_env_labels_history() {
        let raw = r#"{"config":{"Env":["A=1","AWS_SECRET_ACCESS_KEY=zz=="],"Labels":{"auth.user":"u","name":"n"}},
            "history":[{"created_by":"RUN X_TOKEN=t y"},{"empty_layer":true}],
            "annotations":{"org.x.token":"t","org.x.title":"ok"}}"#;
        let out = mask_config_json(raw);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["config"]["Env"][0], "A=1");
        assert_eq!(
            v["config"]["Env"][1],
            format!("AWS_SECRET_ACCESS_KEY={MASK}")
        );
        assert_eq!(v["config"]["Labels"]["auth.user"], MASK);
        assert_eq!(v["config"]["Labels"]["name"], "n");
        assert_eq!(
            v["history"][0]["created_by"],
            format!("RUN X_TOKEN={MASK} y")
        );
        assert_eq!(v["annotations"]["org.x.token"], MASK);
        assert_eq!(v["annotations"]["org.x.title"], "ok");
        assert!(out.contains('\n'), "pretty-printed");
        assert!(!out.contains("zz=="));
    }

    #[test]
    fn legacy_container_config_masked() {
        let raw = r##"{"container_config":{"Env":["X_TOKEN=abc","B=1"],"Labels":{"my.secret":"abc"},
            "Cmd":["/bin/sh","-c","#(nop) ENV X_TOKEN=abc"]}}"##;
        let out = mask_config_json(raw);
        assert!(!out.contains("abc"), "{out}");
        assert!(out.contains("B=1"));
    }

    #[test]
    fn manifest_annotations_masked() {
        let raw = r#"{"config":{"annotations":{"auth.token":"zzz"}},
            "layers":[{"annotations":{"auth.token":"zzz","title":"keep"}}],
            "manifests":[{"annotations":{"auth.token":"zzz"}}],
            "subject":{"annotations":{"auth.token":"zzz"}}}"#;
        let out = mask_config_json(raw);
        assert!(!out.contains("zzz"), "{out}");
        assert!(out.contains("keep"));
    }

    #[test]
    fn invalid_json_unchanged() {
        assert_eq!(
            mask_config_json("not json PASSWORD=x"),
            "not json PASSWORD=x"
        );
    }
}
