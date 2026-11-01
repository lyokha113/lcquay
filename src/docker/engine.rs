//! Docker Engine push/pull streams: progress-entry mapping and bollard error helpers.

use bollard::auth::DockerCredentials;
use bollard::models::{CreateImageInfo, PushImageInfo};

use super::{EngineAuth, PushStep, UNAVAILABLE};
use crate::domain::{Digest, ItemStep, RegistryError, TransferStep};

/// One progress entry of the push stream. `Ok(None)` for entries that carry nothing to show.
/// An `errorDetail` entry is a failed push.
pub(crate) fn map_push_info(
    info: &PushImageInfo,
    tag: &str,
) -> Result<Option<PushStep>, RegistryError> {
    if let Some(e) = &info.error_detail {
        let m = e.message.as_deref().unwrap_or("unknown error");
        return Err(RegistryError::Provider(format!("docker push failed: {m}")));
    }
    if let Some(p) = &info.progress_detail
        && let (Some(cur), Some(total)) = (p.current, p.total)
        && total > 0
    {
        return Ok(Some(PushStep::Bytes {
            current: u64::try_from(cur).unwrap_or(0),
            total: total as u64,
        }));
    }
    let status = info.status.as_deref().unwrap_or("");
    if status.is_empty() {
        return Ok(None);
    }
    if status == "Pushed" || status == "Layer already exists" || status.starts_with("Mounted from")
    {
        return Ok(Some(PushStep::LayerDone));
    }
    // "<tag>: digest: sha256:<hex> size: <n>"
    if let Some(rest) = status
        .strip_prefix(tag)
        .and_then(|r| r.strip_prefix(": digest: "))
        && let Some(d) = rest.split_whitespace().next()
        && let Ok(d) = Digest::parse(d)
    {
        return Ok(Some(PushStep::Digest(d)));
    }
    Ok(Some(PushStep::Status(status.to_string())))
}

/// One progress entry of the pull (`create_image`) stream as a layer step. `Ok(None)` for
/// entries without a layer id or with an unknown status (`Pulling from`, `Digest:`, `Status:`).
/// The status is checked before `progressDetail`: `Extracting` carries bytes too.
/// An `errorDetail` entry is a failed pull.
pub(crate) fn map_pull_info(info: &CreateImageInfo) -> Result<Option<TransferStep>, RegistryError> {
    if let Some(e) = &info.error_detail {
        let m = e.message.as_deref().unwrap_or("unknown error");
        return Err(RegistryError::Provider(format!("docker pull failed: {m}")));
    }
    let Some(id) = &info.id else {
        return Ok(None);
    };
    let step = match info.status.as_deref().unwrap_or("") {
        "Pulling fs layer" | "Waiting" => ItemStep::Waiting { total: None },
        "Downloading" => match info.progress_detail.as_ref().map(|p| (p.current, p.total)) {
            Some((Some(cur), Some(total))) if total > 0 => ItemStep::Bytes {
                current: u64::try_from(cur).unwrap_or(0),
                total: total as u64,
            },
            _ => return Ok(None),
        },
        "Verifying Checksum" | "Download complete" => ItemStep::Downloaded,
        "Extracting" => ItemStep::Extracting,
        "Pull complete" => ItemStep::Done,
        "Already exists" => ItemStep::Exists,
        _ => return Ok(None),
    };
    Ok(Some(TransferStep {
        id: id.clone(),
        step,
    }))
}

/// Built per call and never logged or formatted: its `Debug` prints the password.
pub(super) fn to_docker_credentials(a: EngineAuth) -> DockerCredentials {
    DockerCredentials {
        username: Some(a.username),
        password: Some(a.password.expose().clone()),
        serveraddress: Some(a.server),
        ..Default::default()
    }
}

/// Could not reach the daemon at all (as opposed to the daemon rejecting the call).
fn unreachable(e: &bollard::errors::Error) -> bool {
    use bollard::errors::Error as E;
    matches!(e, E::IOError { .. } | E::HyperLegacyError { .. })
}

pub(super) fn tag_error(e: bollard::errors::Error) -> RegistryError {
    if unreachable(&e) {
        RegistryError::Provider(UNAVAILABLE.into())
    } else {
        RegistryError::Provider(format!("docker tag failed: {e}"))
    }
}

pub(super) fn push_error(e: bollard::errors::Error) -> RegistryError {
    stream_error(e, "push")
}

pub(super) fn pull_error(e: bollard::errors::Error) -> RegistryError {
    stream_error(e, "pull")
}

fn stream_error(e: bollard::errors::Error, verb: &str) -> RegistryError {
    use bollard::errors::Error as E;
    match e {
        // bollard folds `errorDetail` entries into this variant before we see them.
        E::DockerStreamError { error } => {
            RegistryError::Provider(format!("docker {verb} failed: {error}"))
        }
        e if unreachable(&e) => RegistryError::Provider(UNAVAILABLE.into()),
        e => RegistryError::Provider(format!("docker {verb} failed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use bollard::models::{ErrorDetail, ProgressDetail};

    use super::*;
    use crate::auth::{Credential, Secret};
    use crate::docker::engine_auth;

    fn status(s: &str) -> PushImageInfo {
        PushImageInfo {
            status: Some(s.into()),
            ..Default::default()
        }
    }

    #[test]
    fn map_error_detail_fails() {
        let msg = "denied: requested access to the resource is denied";
        let info = PushImageInfo {
            error_detail: Some(ErrorDetail {
                code: None,
                message: Some(msg.into()),
            }),
            ..Default::default()
        };
        match map_push_info(&info, "1.0.1") {
            Err(RegistryError::Provider(m)) => assert!(m.contains(msg), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn push_error_stream_error_keeps_the_daemon_text() {
        let msg = "denied: requested access to the resource is denied";
        let e = bollard::errors::Error::DockerStreamError { error: msg.into() };
        match push_error(e) {
            RegistryError::Provider(m) => assert_eq!(m, format!("docker push failed: {msg}")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn map_layer_done_statuses() {
        for s in [
            "Pushed",
            "Layer already exists",
            "Mounted from library/busybox",
        ] {
            assert_eq!(
                map_push_info(&status(s), "t").unwrap(),
                Some(PushStep::LayerDone),
                "{s}"
            );
        }
    }

    #[test]
    fn map_bytes() {
        let with = |total| PushImageInfo {
            status: Some("Pushing".into()),
            progress_detail: Some(ProgressDetail {
                current: Some(10),
                total,
            }),
            ..Default::default()
        };
        assert_eq!(
            map_push_info(&with(Some(100)), "t").unwrap(),
            Some(PushStep::Bytes {
                current: 10,
                total: 100
            })
        );
        for total in [None, Some(0)] {
            assert_eq!(
                map_push_info(&with(total), "t").unwrap(),
                Some(PushStep::Status("Pushing".into()))
            );
        }
    }

    #[test]
    fn map_digest_line() {
        let hex = "a".repeat(64);
        let line = format!("1.0.1: digest: sha256:{hex} size: 528");
        assert_eq!(
            map_push_info(&status(&line), "1.0.1").unwrap(),
            Some(PushStep::Digest(
                Digest::parse(&format!("sha256:{hex}")).unwrap()
            ))
        );
    }

    #[test]
    fn map_other_status() {
        assert_eq!(
            map_push_info(&status("Preparing"), "t").unwrap(),
            Some(PushStep::Status("Preparing".into()))
        );
        assert_eq!(map_push_info(&status(""), "t").unwrap(), None);
        assert_eq!(map_push_info(&PushImageInfo::default(), "t").unwrap(), None);
    }

    #[test]
    fn map_pull_info_layer_states() {
        let entry = |id: Option<&str>, s: &str, detail: Option<(i64, i64)>| CreateImageInfo {
            id: id.map(str::to_string),
            status: Some(s.into()),
            progress_detail: detail.map(|(c, t)| ProgressDetail {
                current: Some(c),
                total: Some(t),
            }),
            ..Default::default()
        };
        let layer = |s: &str, detail| {
            map_pull_info(&entry(Some("aaa"), s, detail))
                .unwrap()
                .map(|t| (t.id, t.step))
        };
        let waiting = ItemStep::Waiting { total: None };
        let cases = [
            ("Pulling fs layer", None, waiting.clone()),
            ("Waiting", None, waiting),
            (
                "Downloading",
                Some((10, 100)),
                ItemStep::Bytes {
                    current: 10,
                    total: 100,
                },
            ),
            ("Verifying Checksum", None, ItemStep::Downloaded),
            ("Download complete", None, ItemStep::Downloaded),
            ("Extracting", Some((5, 100)), ItemStep::Extracting),
            ("Pull complete", None, ItemStep::Done),
            ("Already exists", None, ItemStep::Exists),
        ];
        for (s, detail, want) in cases {
            assert_eq!(layer(s, detail), Some(("aaa".into(), want)), "{s}");
        }
        let hex = "b".repeat(64);
        for s in [
            "Pulling from x".to_string(),
            format!("Digest: sha256:{hex}"),
            "Status: Downloaded newer image for x:1".to_string(),
        ] {
            assert_eq!(map_pull_info(&entry(None, &s, None)).unwrap(), None, "{s}");
        }
        // Docker sends the tag as the id of "Pulling from".
        let from = entry(Some("1.0"), "Pulling from x", None);
        assert_eq!(map_pull_info(&from).unwrap(), None);
        assert_eq!(map_pull_info(&CreateImageInfo::default()).unwrap(), None);
        let err = CreateImageInfo {
            id: Some("aaa".into()),
            error_detail: Some(ErrorDetail {
                code: None,
                message: Some("manifest unknown".into()),
            }),
            ..Default::default()
        };
        match map_pull_info(&err) {
            Err(RegistryError::Provider(m)) => {
                assert_eq!(m, "docker pull failed: manifest unknown")
            }
            other => panic!("{other:?}"),
        }
        let e = bollard::errors::Error::DockerStreamError {
            error: "denied".into(),
        };
        assert_eq!(
            pull_error(e),
            RegistryError::Provider("docker pull failed: denied".into())
        );
    }

    #[test]
    fn engine_auth_mapping() {
        let basic = Credential::Basic {
            username: "AWS".into(),
            password: Secret("hunter2-token".into()),
        };
        let auth = engine_auth(&basic, "h:5000").unwrap().unwrap();
        assert_eq!(
            (auth.username.as_str(), auth.server.as_str()),
            ("AWS", "h:5000")
        );
        assert_eq!(auth.password.expose(), "hunter2-token");
        assert!(!format!("{auth:?}").contains("hunter2-token"));
        assert!(engine_auth(&Credential::Anonymous, "h").unwrap().is_none());
        let bearer = Credential::Bearer(Secret("t".into()));
        assert!(matches!(
            engine_auth(&bearer, "h"),
            Err(RegistryError::Internal(_))
        ));
    }
}
