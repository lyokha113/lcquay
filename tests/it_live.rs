//! Opt-in live tests (spec §10). All are `#[ignore]` and return early unless `LCQUAY_IT=1`.
//! Endpoints come from env vars; see docs/step1-acceptance.md for the exact setup commands.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use futures::StreamExt;
use lcquay::app::planner;
use lcquay::app::state::{Action, CacheKey, CacheValue, Effect, Event, RequestId, State};
use lcquay::app::update::{Input, update};
use lcquay::app::write::OpId;
use lcquay::config;
use lcquay::docker::{BollardEngine, Engine, PushStep};
use lcquay::domain::{
    ArtifactStatus, DeleteReport, Details, Digest, ManifestKind, Marks, OpOutcome, Reference,
    RepoPath,
};
use lcquay::providers::{DELETION_DISABLED, Registry, build_registries};
use lcquay::reqlog::RequestLog;
use lcquay::tasks::{Clipboard, Runtime, view};
use time::OffsetDateTime;

const REPO: &str = "lcquay-it/busybox";
const TAG: &str = "1";

/// `None` (with a printed note) when `LCQUAY_IT` is unset. Under `LCQUAY_IT=1` a missing env var
/// panics, so a typo can never produce a silent green.
fn gate(test: &str, vars: &[&str]) -> Option<Vec<String>> {
    if std::env::var("LCQUAY_IT").as_deref() != Ok("1") {
        println!("{test}: skipped (LCQUAY_IT != 1)");
        return None;
    }
    let missing: Vec<_> = vars.iter().filter(|v| std::env::var(v).is_err()).collect();
    assert!(
        missing.is_empty(),
        "{test}: LCQUAY_IT=1 but missing env vars: {missing:?}"
    );
    Some(vars.iter().map(|v| std::env::var(v).unwrap()).collect())
}

/// Value of the details row labelled `k`.
fn field<'a>(d: &'a Details, k: &str) -> Option<&'a str> {
    d.fields
        .iter()
        .find(|(l, _)| l == k)
        .map(|(_, v)| v.as_str())
}

async fn registry(toml: &str) -> Registry {
    let cfg = config::parse(toml, Path::new("it_live.toml")).expect("config");
    build_registries(&cfg, &RequestLog::default())
        .await
        .expect("build_registries")
        .pop()
        .expect("one registry")
}

fn oci_toml(endpoint: &str, auth: &str) -> String {
    format!(
        "schema_version = 1\n[[registries]]\nid = \"it\"\nprovider = {{ type = \"oci\" }}\n\
         endpoint = \"{endpoint}\"\nauth = {{ type = \"{auth}\" }}\n"
    )
}

fn noop(_: usize) {}

/// Repos contain `lcquay-it/busybox`, inventory has tag `1`, manifest and config load.
async fn assert_browse(reg: &Registry) {
    let repo = RepoPath(REPO.into());
    let repos = reg
        .provider
        .repositories(&noop)
        .await
        .expect("repositories");
    assert!(repos.iter().any(|r| r.path == repo), "repos: {repos:?}");

    let inv = reg
        .provider
        .artifacts(&repo, &noop)
        .await
        .expect("inventory");
    let art = inv
        .iter()
        .find(|a| a.tags.iter().any(|t| t == TAG))
        .unwrap_or_else(|| panic!("no tag {TAG} in {inv:?}"));

    let m = reg
        .oci
        .manifest(&repo, &Reference::Digest(art.digest.clone()))
        .await
        .expect("manifest");
    let ManifestKind::Image { config, layers } = &m.kind else {
        panic!("expected an image manifest, got {:?}", m.kind);
    };
    assert!(!layers.is_empty());
    let cfg = reg
        .oci
        .image_config(&repo, config)
        .await
        .expect("config")
        .expect("image config");
    assert!(
        cfg.platform.is_some(),
        "config has no platform: {}",
        cfg.raw
    );
}

#[tokio::test]
#[ignore]
async fn registry2_anonymous_browse() {
    let Some(v) = gate("registry2_anonymous_browse", &["LCQUAY_IT_REGISTRY"]) else {
        return;
    };
    assert_browse(&registry(&oci_toml(&v[0], "anonymous")).await).await;
}

/// Credentials come from the Docker config: run with `DOCKER_CONFIG` pointing at a temp dir whose
/// `config.json` has `auths` for the registry host (see docs/step1-acceptance.md).
#[tokio::test]
#[ignore]
async fn registry2_htpasswd_basic() {
    let Some(v) = gate("registry2_htpasswd_basic", &["LCQUAY_IT_REGISTRY_AUTH"]) else {
        return;
    };
    assert_browse(&registry(&oci_toml(&v[0], "docker-config")).await).await;
}

/// Password comes from `LCQUAY_IT_BASIC_PW` (see docs/step1-acceptance.md).
#[tokio::test]
#[ignore]
async fn registry2_htpasswd_basic_env() {
    let Some(v) = gate(
        "registry2_htpasswd_basic_env",
        &[
            "LCQUAY_IT_REGISTRY_AUTH",
            "LCQUAY_IT_BASIC_USER",
            "LCQUAY_IT_BASIC_PW",
        ],
    ) else {
        return;
    };
    let toml = format!(
        "schema_version = 1\n[[registries]]\nid = \"it\"\nprovider = {{ type = \"oci\" }}\n\
         endpoint = \"{}\"\nauth = {{ type = \"basic\", username = \"{}\", password_env = \"LCQUAY_IT_BASIC_PW\" }}\n",
        v[0], v[1]
    );
    assert_browse(&registry(&toml).await).await;
}

/// Tags the local image `image` as `<host>/<repo>:<unix-seconds>` with the real Engine and pushes
/// it anonymously. Returns the tag and the digest from the push stream.
async fn push_new(reg: &Registry, image: &str, repo: &str) -> (String, Digest) {
    let engine = BollardEngine::connect().expect("docker engine");
    let target = format!("{}/{repo}", reg.cfg.ref_host);
    let tag = OffsetDateTime::now_utc().unix_timestamp().to_string();

    engine.tag(image, &target, &tag).await.expect("tag");
    let mut stream = engine.push(&target, &tag, None);
    let mut digest = None;
    while let Some(step) = stream.next().await {
        if let PushStep::Digest(d) = step.expect("push step") {
            digest = Some(d);
        }
    }
    (tag, digest.expect("push stream ended without a Digest"))
}

/// Real Docker Engine tags the local image `LCQUAY_IT_PUSH_IMAGE` (e.g. `busybox:1`) into the
/// registry, pushes it anonymously, and the new tag shows up in the inventory. Needs a running
/// daemon that treats the registry host as insecure (127.0.0.0/8 and localhost are by default).
#[tokio::test]
#[ignore]
async fn registry2_push() {
    let Some(v) = gate(
        "registry2_push",
        &["LCQUAY_IT_REGISTRY", "LCQUAY_IT_PUSH_IMAGE"],
    ) else {
        return;
    };
    let reg = registry(&oci_toml(&v[0], "anonymous")).await;
    let (tag, digest) = push_new(&reg, &v[1], "lcquay-it/push").await;

    let inv = reg
        .provider
        .artifacts(&RepoPath("lcquay-it/push".into()), &noop)
        .await
        .expect("inventory");
    let row = inv.iter().find(|a| a.tags.contains(&tag));
    assert!(row.is_some(), "tag {tag} ({digest}) not in {inv:?}");
    assert_eq!(
        row.map(|a| &a.digest),
        Some(&digest),
        "inventory digest differs from the push stream's"
    );
}

/// Pushes as `registry2_push`, then pulls the same image back by digest reference. No image is
/// removed: the pull only re-resolves what the daemon already has, so user images stay intact.
#[tokio::test]
#[ignore]
async fn registry2_pull() {
    let Some(v) = gate(
        "registry2_pull",
        &["LCQUAY_IT_REGISTRY", "LCQUAY_IT_PUSH_IMAGE"],
    ) else {
        return;
    };
    let reg = registry(&oci_toml(&v[0], "anonymous")).await;
    let (_, digest) = push_new(&reg, &v[1], "lcquay-it/pull").await;
    let engine = BollardEngine::connect().expect("docker engine");
    let image = format!("{}/lcquay-it/pull@{digest}", reg.cfg.ref_host);
    let mut stream = engine.pull(&image, None, None);
    while let Some(step) = stream.next().await {
        step.expect("pull step");
    }
}

fn req_of(fx: &[Effect], key: &CacheKey) -> RequestId {
    fx.iter()
        .find_map(|e| match e {
            Effect::Fetch { key: k, req } if k == key => Some(*req),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no Fetch for {key:?} in {fx:?}"))
}

/// Real ECR registry: repositories and inventory load, and walking every image row (archived
/// ones included) through the real `update` never requests manifest/config/referrers for an
/// archived digest (spec §13 row 1).
#[tokio::test]
#[ignore]
async fn ecr_browse() {
    let Some(v) = gate(
        "ecr_browse",
        &[
            "LCQUAY_IT_ECR_ACCOUNT",
            "LCQUAY_IT_ECR_REGION",
            "LCQUAY_IT_ECR_REPO",
        ],
    ) else {
        return;
    };
    let toml = format!(
        "schema_version = 1\n[[registries]]\nid = \"it\"\n\
         provider = {{ type = \"ecr\", account_id = \"{}\", region = \"{}\" }}\n",
        v[0], v[1]
    );
    let reg = registry(&toml).await;
    let repo = RepoPath(v[2].clone());

    let repos = reg
        .provider
        .repositories(&noop)
        .await
        .expect("repositories");
    assert!(repos.iter().any(|r| r.path == repo), "repos: {repos:?}");
    let rd = reg
        .provider
        .registry_details()
        .await
        .expect("registry details");
    assert_eq!(field(&rd, "Registry"), Some(v[0].as_str()), "{rd:?}");
    assert!(
        field(&rd, "Caller").is_some_and(|c| c.starts_with("arn:aws:")),
        "{rd:?}"
    );
    let pd = reg
        .provider
        .repo_details(&repo)
        .await
        .expect("repo details");
    assert!(
        field(&pd, "ARN").is_some_and(|a| a.ends_with(&format!(":repository/{repo}"))),
        "{pd:?}"
    );
    assert!(
        field(&pd, "URI").is_some_and(|u| u.ends_with(&format!("/{repo}"))),
        "{pd:?}"
    );
    let inv = reg
        .provider
        .artifacts(&repo, &noop)
        .await
        .expect("inventory");
    assert!(!inv.is_empty(), "repository {repo} has no images");
    let archived: Vec<_> = inv
        .iter()
        .filter(|a| a.status == ArtifactStatus::Archived)
        .map(|a| a.digest.clone())
        .collect();
    println!(
        "ecr_browse: {} images, {} archived",
        inv.len(),
        archived.len()
    );
    if archived.is_empty() {
        if std::env::var("LCQUAY_IT_ECR_ALLOW_NO_ARCHIVED").as_deref() == Ok("1") {
            println!("ecr_browse: no archived image; archived check SKIPPED");
        } else {
            panic!(
                "repository {repo} has no archived image, so the archived check would be vacuous; \
                 pick another repo or set LCQUAY_IT_ECR_ALLOW_NO_ARCHIVED=1"
            );
        }
    }
    let active: Vec<_> = inv
        .iter()
        .filter(|a| a.status != ArtifactStatus::Archived)
        .map(|a| a.digest.clone())
        .collect();

    let id = reg.id.clone();
    let now = OffsetDateTime::now_utc();
    let ev = |key, req, value| {
        Input::Event(Event::Loaded {
            key,
            req,
            result: Ok(value),
        })
    };
    let (mut s, fx) = State::new(vec![view(&reg)]);
    let mut all: Vec<Effect> = fx.clone();
    let k = CacheKey::Repos(id.clone());
    all.extend(update(
        &mut s,
        ev(k.clone(), req_of(&fx, &k), CacheValue::Repos(repos)),
        now,
    ));
    all.extend(update(&mut s, Input::Action(Action::Enter), now)); // Repos pane
    for _ in 0..1000 {
        if s.selected_repo().is_some_and(|r| r.path == repo) {
            break;
        }
        all.extend(update(&mut s, Input::Action(Action::Down), now));
    }
    assert_eq!(s.selected_repo().map(|r| &r.path), Some(&repo));
    let k = CacheKey::Inventory(id, repo.clone());
    let fx = update(
        &mut s,
        ev(k.clone(), req_of(&all, &k), CacheValue::Inventory(inv)),
        now,
    );
    all.extend(fx);
    all.extend(update(&mut s, Input::Action(Action::Enter), now)); // Images pane
    for _ in 0..s.visible_images().len() {
        all.extend(update(&mut s, Input::Action(Action::Down), now));
    }

    // Positive control: content is fetched for non-archived rows, so the walk really exercised it.
    let fetched_active = all.iter().any(|e| {
        matches!(e, Effect::Fetch { key: CacheKey::Manifest(_, _, d), .. } if active.contains(d))
    });
    assert!(
        fetched_active,
        "no Manifest fetch for any non-archived row ({} active); the walk proved nothing",
        active.len()
    );
    for e in &all {
        if let Effect::Fetch {
            key:
                CacheKey::Manifest(_, _, d) | CacheKey::Config(_, _, d) | CacheKey::Referrers(_, _, d),
            ..
        } = e
        {
            assert!(
                !archived.contains(d),
                "content fetch for archived {d}: {e:?}"
            );
        }
    }

    // OCI content path (Basic AWS:<pw> challenge, manifest, config blob redirect, referrers).
    let d = active
        .first()
        .expect("no active image to fetch content for");
    let m = reg
        .oci
        .manifest(&repo, &Reference::Digest(d.clone()))
        .await
        .expect("manifest");
    let image_cfg = match &m.kind {
        ManifestKind::Image { config, .. } => Some(config.clone()),
        ManifestKind::Index { manifests } => {
            let child = manifests.first().expect("index without children");
            let cm = reg
                .oci
                .manifest(&repo, &Reference::Digest(child.digest.clone()))
                .await
                .expect("child manifest");
            match cm.kind {
                ManifestKind::Image { config, .. } => Some(config),
                ManifestKind::Index { .. } => None,
            }
        }
    };
    if let Some(c) = image_cfg {
        let cfg = reg.oci.image_config(&repo, &c).await.expect("image_config");
        // None only for non-image artifacts (non-config media type).
        assert!(
            cfg.is_some() || m.artifact_type.is_some(),
            "image manifest {d} has no image config (media type {})",
            c.media_type
        );
    }
    reg.oci.referrers(&repo, d).await.expect("referrers");
}

/// Real ECR Public registry: repos carry the alias prefix, the inventory is non-empty, and a
/// manifest fetch exercises the Basic -> `public.ecr.aws/token/` exchange (spec §13).
#[tokio::test]
#[ignore]
async fn ecr_public_browse() {
    let Some(v) = gate(
        "ecr_public_browse",
        &["LCQUAY_IT_ECR_PUBLIC_ALIAS", "LCQUAY_IT_ECR_PUBLIC_REPO"],
    ) else {
        return;
    };
    let toml = format!(
        "schema_version = 1\n[[registries]]\nid = \"it\"\n\
         provider = {{ type = \"ecr-public\", alias = \"{}\" }}\n",
        v[0]
    );
    let reg = registry(&toml).await;
    let repo = RepoPath(format!("{}/{}", v[0], v[1]));

    let repos = reg
        .provider
        .repositories(&noop)
        .await
        .expect("repositories");
    assert!(repos.iter().any(|r| r.path == repo), "repos: {repos:?}");
    let rd = reg
        .provider
        .registry_details()
        .await
        .expect("registry details");
    assert_eq!(field(&rd, "Alias"), Some(v[0].as_str()), "{rd:?}");
    assert!(
        matches!(field(&rd, "Verified"), Some("yes" | "no")),
        "{rd:?}"
    );
    let pd = reg
        .provider
        .repo_details(&repo)
        .await
        .expect("repo details");
    assert_eq!(
        field(&pd, "URI"),
        Some(format!("public.ecr.aws/{repo}").as_str()),
        "{pd:?}"
    );
    let inv = reg
        .provider
        .artifacts(&repo, &noop)
        .await
        .expect("inventory");
    assert!(!inv.is_empty(), "repository {repo} has no images");
    let art = inv
        .iter()
        .find(|a| !a.tags.is_empty())
        .unwrap_or_else(|| panic!("no tagged image in {inv:?}"));
    reg.oci
        .manifest(&repo, &Reference::Digest(art.digest.clone()))
        .await
        .expect("manifest");
}

struct NoClip;

impl Clipboard for NoClip {
    fn set(&mut self, _: String) -> Result<(), String> {
        Ok(())
    }
}

/// Next runtime event, failing the test after 60 s instead of hanging.
async fn next(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Event>) -> Event {
    tokio::time::timeout(Duration::from_secs(60), rx.recv())
        .await
        .expect("no runtime event in 60 s")
        .expect("runtime channel closed")
}

/// Pushes `image` as `<repo>:<unix-seconds>`, then runs the real snapshot -> plan -> delete path
/// with every tag of that digest marked. Returns the tag and the delete report.
async fn push_and_delete(reg: Registry, image: &str, repo: &str) -> (String, DeleteReport) {
    let (tag, digest) = push_new(&reg, image, repo).await;
    let (path, id, caps) = (RepoPath(repo.into()), reg.id.clone(), reg.provider.caps());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let rt = Runtime::new(vec![reg], None, Box::new(NoClip), tx);

    rt.run(Effect::DeleteSnapshot {
        op: OpId(1),
        registry: id.clone(),
        repo: path.clone(),
        marked: BTreeSet::from([digest.clone()]),
    });
    let Event::DeleteSnapshot { result, .. } = next(&mut rx).await else {
        panic!("want DeleteSnapshot")
    };
    let snap = result.expect("snapshot");

    // Every tag of the digest: earlier runs of the disabled registry leave tags behind, and the
    // same image always has the same digest. `whole` lets a registry without `delete_tag` plan it.
    let mut marks = Marks::default();
    let tags = snap
        .inventory
        .iter()
        .find(|a| a.digest == digest)
        .map(|a| a.tags.iter().cloned().collect())
        .unwrap_or_default();
    marks.tags.insert(digest, tags);
    let plan = planner::plan(&marks, &snap, caps, true);
    assert!(!plan.ops().is_empty(), "empty plan: {plan:?}");

    // The pushed image is a single manifest, so no parent lies inside the delete set.
    rt.run(Effect::Delete {
        op: OpId(2),
        registry: id,
        repo: path,
        plan,
        parents: Default::default(),
    });
    let Event::DeleteDone { report, .. } = next(&mut rx).await else {
        panic!("want DeleteDone")
    };
    (tag, report)
}

/// Delete-enabled `registry:2` (`REGISTRY_STORAGE_DELETE_ENABLED=true`): the pushed image is
/// deleted through snapshot -> plan -> delete and its tag leaves the inventory. Then the default
/// delete-disabled registry: the run aborts as unsupported with `DELETION_DISABLED`.
#[tokio::test]
#[ignore]
async fn registry2_delete() {
    let Some(v) = gate(
        "registry2_delete",
        &[
            "LCQUAY_IT_REGISTRY_DELETE",
            "LCQUAY_IT_REGISTRY",
            "LCQUAY_IT_PUSH_IMAGE",
        ],
    ) else {
        return;
    };
    let repo = "lcquay-it/delete";

    let reg = registry(&oci_toml(&v[0], "anonymous")).await;
    let provider = reg.provider.clone();
    let (tag, report) = push_and_delete(reg, &v[2], repo).await;
    assert!(!report.aborted_unsupported, "{report:?}");
    assert!(
        report.results.iter().all(|(_, o)| *o == OpOutcome::Done),
        "{report:?}"
    );
    let inv = provider
        .artifacts(&RepoPath(repo.into()), &noop)
        .await
        .expect("inventory");
    assert!(
        inv.iter().all(|a| !a.tags.contains(&tag)),
        "tag {tag} still listed: {inv:?}"
    );

    let reg = registry(&oci_toml(&v[1], "anonymous")).await;
    let (_, report) = push_and_delete(reg, &v[2], repo).await;
    assert!(report.aborted_unsupported, "{report:?}");
    assert_eq!(
        report.results.iter().map(|(_, o)| o).collect::<Vec<_>>(),
        [&OpOutcome::Failed(DELETION_DISABLED.into())],
        "{report:?}"
    );
}

/// Copy engine against ECR. The user creates the scratch repo `LCQUAY_IT_ECR_SCRATCH_REPO` before
/// and deletes it after. `LCQUAY_IT_ECR_REPO` must hold a tagged image with a layer over 10 MiB:
/// its largest tagged image is copied into the scratch repo as `it-<unix time>` with a 5 MiB
/// chunk request and no mount attempt. ECR answers the upload `POST` with `OCI-Chunk-Min-Length:
/// 10485760` and the engine raises the chunk to that, so parts are 10 MiB: only layers over
/// 10 MiB go up as at least two `PATCH` parts and an empty `PUT`; smaller blobs go monolithic
/// (`POST` + `PUT`). Then that tag is retagged in the scratch repo
/// (`same_registry`). `copy_image` verifies each digest. Both new tags are deleted before any
/// assertion runs, so a failed check leaves nothing behind.
#[tokio::test]
#[ignore]
async fn ecr_copy_scratch() {
    use lcquay::domain::DeleteOp;
    use lcquay::registry::copy::{CopyJob, copy_image};
    use std::sync::atomic::AtomicBool;

    let Some(v) = gate(
        "ecr_copy_scratch (LCQUAY_IT_ECR_REPO needs a tagged image with a layer over 10 MiB)",
        &[
            "LCQUAY_IT_ECR_ACCOUNT",
            "LCQUAY_IT_ECR_REGION",
            "LCQUAY_IT_ECR_REPO",
            "LCQUAY_IT_ECR_SCRATCH_REPO",
        ],
    ) else {
        return;
    };
    let toml = format!(
        "schema_version = 1\n[[registries]]\nid = \"it\"\n\
         provider = {{ type = \"ecr\", account_id = \"{}\", region = \"{}\" }}\n",
        v[0], v[1]
    );
    let log = RequestLog::default();
    let cfg = config::parse(&toml, Path::new("it_live.toml")).expect("config");
    let reg = build_registries(&cfg, &log)
        .await
        .expect("build_registries")
        .pop()
        .expect("one registry");
    let (src, scratch) = (RepoPath(v[2].clone()), RepoPath(v[3].clone()));
    let inv = reg
        .provider
        .artifacts(&src, &noop)
        .await
        .expect("inventory");
    let img = inv
        .iter()
        .filter(|a| a.status == ArtifactStatus::Active && !a.tags.is_empty())
        .max_by_key(|a| a.size)
        .expect("no tagged image in the source repo");
    let tag = format!("it-{}", OffsetDateTime::now_utc().unix_timestamp());
    let retag = format!("{tag}-retag");
    let flag = AtomicBool::new(false);

    let job = CopyJob {
        src: &reg.oci,
        src_repo: &src,
        dst: &reg.oci,
        dst_repo: &scratch,
        digest: &img.digest,
        tag: &tag,
        same_registry: false,
        chunk: 5 * 1024 * 1024,
        manifest_sent: &flag,
    };
    let retag_job = CopyJob {
        src_repo: &scratch,
        tag: &retag,
        same_registry: true,
        ..job
    };
    let copied = copy_image(&job, &|_| {}).await;
    let entries = log.snapshot();
    let retagged = match copied {
        Ok(_) => Some(copy_image(&retag_job, &|_| {}).await),
        Err(_) => None,
    };

    // Clean up first: a failed assertion below must not leave tags in the scratch repo. A tag
    // that was never created just fails its own op.
    let ops = [tag.clone(), retag.clone()].map(|tag| DeleteOp::RemoveTag {
        tag,
        expected: img.digest.clone(),
    });
    let cleanup = reg.provider.delete(&scratch, &ops).await;

    copied.expect("copy");
    let patches = entries.iter().filter(|e| e.op == "PATCH upload").count();
    assert!(
        patches >= 2,
        "{patches} PATCH: pick a source image with a layer over 10 MiB"
    );
    assert!(entries.iter().all(|e| !e.path.contains('?')), "{entries:?}");
    retagged.expect("retag ran").expect("retag");
    println!(
        "ecr_copy_scratch: copied {} as {tag} ({patches} PATCH) and {retag}",
        img.digest
    );
    let (report, stopped) = cleanup.expect("delete");
    assert!(
        stopped.is_none() && report.results.iter().all(|(_, o)| *o == OpOutcome::Done),
        "{report:?}"
    );
}

/// Copy wizard runtime (`Effect::CopyImage`) against two `registry:2` instances (`LCQUAY_IT_REGISTRY`,
/// `LCQUAY_IT_REGISTRY2`, both anonymous). A fresh image is pushed to the first, then copied three
/// ways: a retag in the same repo, a copy to another repo of the same registry (blob mount) and a
/// copy to the second registry (upload). Each destination tag's `HEAD` digest is the source digest.
#[tokio::test]
#[ignore]
async fn registry2_copy() {
    use lcquay::app::transfer::TransferOutcome;

    let Some(v) = gate(
        "registry2_copy",
        &[
            "LCQUAY_IT_REGISTRY",
            "LCQUAY_IT_REGISTRY2",
            "LCQUAY_IT_PUSH_IMAGE",
        ],
    ) else {
        return;
    };
    let toml = format!(
        "schema_version = 1\n\
         [[registries]]\nid = \"it\"\nprovider = {{ type = \"oci\" }}\nendpoint = \"{}\"\nauth = {{ type = \"anonymous\" }}\n\
         [[registries]]\nid = \"it2\"\nprovider = {{ type = \"oci\" }}\nendpoint = \"{}\"\nauth = {{ type = \"anonymous\" }}\n",
        v[0], v[1]
    );
    let cfg = config::parse(&toml, Path::new("it_live.toml")).expect("config");
    let regs = build_registries(&cfg, &RequestLog::default())
        .await
        .expect("build_registries");
    let src = RepoPath("lcquay-it/copy".into());
    let (tag, digest) = push_new(&regs[0], &v[2], &src.0).await;
    let clients: Vec<_> = regs.iter().map(|r| (r.id.clone(), r.oci.clone())).collect();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let rt = Runtime::new(regs, None, Box::new(NoClip), tx);

    let retag = format!("{tag}-retag");
    let cases = [
        (0, "lcquay-it/copy", retag.as_str()),
        (0, "lcquay-it/copy-mount", tag.as_str()),
        (1, "lcquay-it/copy", tag.as_str()),
    ];
    for (n, (dst, dst_repo, dst_tag)) in cases.into_iter().enumerate() {
        let op = OpId(n as u64 + 1);
        let (dst_id, client) = &clients[dst];
        let dst_repo = RepoPath(dst_repo.into());
        rt.run(Effect::CopyImage {
            op,
            src: clients[0].0.clone(),
            src_repo: src.clone(),
            digest: digest.clone(),
            dst: dst_id.clone(),
            dst_repo: dst_repo.clone(),
            tag: dst_tag.into(),
        });
        let outcome = loop {
            match next(&mut rx).await {
                Event::CopyProgress { .. } => {}
                Event::CopyDone { op: o, outcome } if o == op => break outcome,
                e => panic!("unexpected event {e:?}"),
            }
        };
        assert_eq!(
            outcome,
            TransferOutcome::Done,
            "{dst_id}/{dst_repo}:{dst_tag}"
        );
        let (got, _) = client
            .head_manifest(&dst_repo, &Reference::Tag(dst_tag.into()))
            .await
            .expect("head");
        assert_eq!(got, digest, "{dst_id}/{dst_repo}:{dst_tag}");
    }
}

/// Exports the single-platform `lcquay-it/busybox:1` through the runtime to a temp dir under a
/// fresh reference; `docker load -i` takes the tarball and `docker image inspect` finds it.
#[tokio::test]
#[ignore]
async fn registry2_export_load() {
    use lcquay::app::transfer::TransferOutcome;

    let Some(v) = gate("registry2_export_load", &["LCQUAY_IT_REGISTRY"]) else {
        return;
    };
    let reg = registry(&oci_toml(&v[0], "anonymous")).await;
    let repo = RepoPath(REPO.into());
    let (digest, _) = reg
        .oci
        .head_manifest(&repo, &Reference::Tag(TAG.into()))
        .await
        .expect("head");
    // A reference that exists nowhere yet: only the load can create it.
    let tag = format!("export-{}", OffsetDateTime::now_utc().unix_timestamp());
    let full_ref = format!("{}/lcquay-it/export:{tag}", reg.cfg.ref_host);
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("busybox.tar");
    let id = reg.id.clone();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let rt = Runtime::new(vec![reg], None, Box::new(NoClip), tx);
    rt.run(Effect::Export {
        op: OpId(1),
        registry: id,
        repo,
        top: digest,
        platform: None,
        tag: Some(tag),
        full_ref: full_ref.clone(),
        path: path.to_str().expect("utf-8 path").into(),
        overwrite: false,
    });
    let outcome = loop {
        match next(&mut rx).await {
            Event::ExportProgress { .. } => {}
            Event::ExportDone { outcome, .. } => break outcome,
            e => panic!("unexpected event {e:?}"),
        }
    };
    assert_eq!(outcome, TransferOutcome::Done);

    let docker = |args: &[&str]| {
        std::process::Command::new("docker")
            .args(args)
            .output()
            .expect("docker cli")
    };
    let load = docker(&["load", "-i", path.to_str().expect("utf-8 path")]);
    assert!(
        load.status.success(),
        "docker load: {}",
        String::from_utf8_lossy(&load.stderr)
    );
    let inspect = docker(&["image", "inspect", &full_ref]);
    let _ = docker(&["image", "rm", &full_ref]);
    assert!(
        inspect.status.success(),
        "docker image inspect {full_ref}: {}",
        String::from_utf8_lossy(&inspect.stderr)
    );
}
