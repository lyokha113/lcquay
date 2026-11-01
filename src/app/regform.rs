//! The registry form (`c` edit, `n` new) and the remove prompt (`x`). Pure state: `main` reads
//! and writes the config file on the effects.

use super::state::{Action, Effect, Pane, State, StatusKind};
use super::write::OpId;
use crate::config_edit::{AuthKind, Kind, RegistryForm, validate};
use crate::conntest::{Check, CheckStatus, STEPS};
use crate::domain::RegistryId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    Id,
    Type,
    Pinned,
    Account,
    Region,
    Profile,
    Alias,
    Endpoint,
    RefHost,
    Auth,
    Username,
    PasswordEnv,
    Repos,
    TrustedRealms,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RegForm {
    /// The registry being edited; `None` for a new one.
    pub original: Option<RegistryId>,
    /// The values the form opened with (`dirty` compares against them).
    pub initial: RegistryForm,
    pub values: RegistryForm,
    pub focus: Field,
    /// Esc on a dirty form: `discard changes? y/n` is shown.
    pub confirm_discard: bool,
    pub error: Option<String>,
    /// The last `Ctrl-T` connection test.
    pub test: Option<ConnTest>,
}

/// A connection test: one slot per `conntest::STEPS`, `None` until that step reports.
#[derive(Clone, Debug, PartialEq)]
pub struct ConnTest {
    pub op: OpId,
    pub checks: [Option<Check>; 5],
}

impl ConnTest {
    /// `None` while running; `Some(true)` only when every step is `Ok`.
    pub fn verdict(&self) -> Option<bool> {
        self.checks.iter().try_fold(true, |all, c| {
            c.as_ref().map(|c| all && c.status == CheckStatus::Ok)
        })
    }

    pub fn verdict_text(&self) -> Option<&'static str> {
        self.verdict().map(|ok| {
            if ok {
                "Ready"
            } else {
                "Not working — save anyway with Ctrl-S"
            }
        })
    }
}

/// ←→ order of the `Type` and `Auth` choices, with their config names.
pub const KINDS: [(Kind, &str); 3] = [
    (Kind::Ecr, "ecr"),
    (Kind::EcrPublic, "ecr-public"),
    (Kind::Oci, "oci"),
];
pub const AUTHS: [(AuthKind, &str); 3] = [
    (AuthKind::Anonymous, "anonymous"),
    (AuthKind::DockerConfig, "docker-config"),
    (AuthKind::Basic, "basic"),
];

/// The entry after (or before) `cur` in `all`, wrapping.
fn cycle<T: PartialEq + Copy>(all: &[(T, &str)], cur: T, forward: bool) -> T {
    let i = all.iter().position(|(v, _)| *v == cur).unwrap_or(0);
    let n = all.len();
    all[if forward {
        (i + 1) % n
    } else {
        (i + n - 1) % n
    }]
    .0
}

impl RegForm {
    pub fn new(original: Option<RegistryId>, form: RegistryForm) -> RegForm {
        RegForm {
            original,
            initial: form.clone(),
            values: form,
            focus: Field::Id,
            confirm_discard: false,
            error: None,
            test: None,
        }
    }

    /// The fields shown for the current `Type` (and `Auth`), in order.
    pub fn fields(&self) -> Vec<Field> {
        use Field::*;
        match self.values.kind {
            Kind::Ecr => vec![
                Id, Type, Account, Region, Profile, Endpoint, RefHost, Pinned,
            ],
            Kind::EcrPublic => vec![Id, Type, Alias, Profile, Endpoint, RefHost, Pinned],
            Kind::Oci => {
                let mut v = vec![Id, Type, Endpoint, RefHost, Auth];
                if self.values.auth == AuthKind::Basic {
                    v.extend([Username, PasswordEnv]);
                }
                v.extend([Repos, TrustedRealms, Pinned]);
                v
            }
        }
    }

    pub fn dirty(&self) -> bool {
        self.values != self.initial
    }

    pub fn label(f: Field, kind: Kind) -> &'static str {
        match f {
            Field::Id => "Id",
            Field::Type => "Type",
            Field::Pinned => "Pinned",
            Field::Account => "Account",
            Field::Region => "Region",
            Field::Profile => "Profile",
            Field::Alias => "Alias",
            Field::Endpoint if kind == Kind::Oci => "Endpoint",
            Field::Endpoint => "Endpoint override",
            Field::RefHost => "Ref host override",
            Field::Auth => "Auth",
            Field::Username => "Username",
            Field::PasswordEnv => "Password env",
            Field::Repos => "Repos",
            Field::TrustedRealms => "Trusted realms",
        }
    }

    /// The typed text of a text field; `None` for the `Type` / `Auth` choices.
    pub fn text(&self, f: Field) -> Option<&str> {
        let v = &self.values;
        Some(match f {
            Field::Id => &v.id,
            Field::Pinned => &v.pinned,
            Field::Account => &v.account_id,
            Field::Region => &v.region,
            Field::Profile => &v.profile,
            Field::Alias => &v.alias,
            Field::Endpoint => &v.endpoint,
            Field::RefHost => &v.ref_host,
            Field::Username => &v.username,
            Field::PasswordEnv => &v.password_env,
            Field::Repos => &v.repositories,
            Field::TrustedRealms => &v.trusted_realms,
            Field::Type | Field::Auth => return None,
        })
    }

    fn text_mut(&mut self, f: Field) -> Option<&mut String> {
        let v = &mut self.values;
        Some(match f {
            Field::Id => &mut v.id,
            Field::Pinned => &mut v.pinned,
            Field::Account => &mut v.account_id,
            Field::Region => &mut v.region,
            Field::Profile => &mut v.profile,
            Field::Alias => &mut v.alias,
            Field::Endpoint => &mut v.endpoint,
            Field::RefHost => &mut v.ref_host,
            Field::Username => &mut v.username,
            Field::PasswordEnv => &mut v.password_env,
            Field::Repos => &mut v.repositories,
            Field::TrustedRealms => &mut v.trusted_realms,
            Field::Type | Field::Auth => return None,
        })
    }

    /// A choice field: the options and the selected one's index.
    pub fn choice(&self, f: Field) -> Option<(Vec<&'static str>, usize)> {
        fn pick<T: PartialEq>(all: &[(T, &'static str)], cur: &T) -> (Vec<&'static str>, usize) {
            let i = all.iter().position(|(v, _)| v == cur).unwrap_or(0);
            (all.iter().map(|(_, n)| *n).collect(), i)
        }
        match f {
            Field::Type => Some(pick(&KINDS, &self.values.kind)),
            Field::Auth => Some(pick(&AUTHS, &self.values.auth)),
            _ => None,
        }
    }

    /// Muted text after the value: the derived endpoint of an empty override, or a short help.
    pub fn hint(&self, f: Field) -> Option<String> {
        let v = &self.values;
        let or = |s: &str, d: &str| {
            if s.trim().is_empty() {
                d.to_owned()
            } else {
                s.trim().to_owned()
            }
        };
        let text = match f {
            Field::Endpoint if !v.endpoint.trim().is_empty() => return None,
            Field::Endpoint => match v.kind {
                Kind::Ecr => format!(
                    "https://{}.dkr.ecr.{}.amazonaws.com (auto)",
                    or(&v.account_id, "<account>"),
                    or(&v.region, "<region>")
                ),
                Kind::EcrPublic => "https://public.ecr.aws (auto)".into(),
                Kind::Oci => return None,
            },
            Field::RefHost if v.ref_host.trim().is_empty() => "empty = the endpoint host".into(),
            Field::Profile => "empty = default chain".into(),
            Field::Repos => "needed when the registry has no catalog".into(),
            Field::PasswordEnv => "env var name; the password is never stored".into(),
            _ => return None,
        };
        Some(text)
    }

    fn step(&mut self, forward: bool) {
        let fields = self.fields();
        let n = fields.len();
        let i = fields.iter().position(|f| *f == self.focus).unwrap_or(0);
        self.focus = fields[if forward {
            (i + 1) % n
        } else {
            (i + n - 1) % n
        }];
    }
}

impl State {
    pub fn reg_form(&self) -> Option<&RegForm> {
        self.form.as_ref()
    }

    /// The registry the `x` prompt asks about.
    pub fn remove_confirm(&self) -> Option<&RegistryId> {
        self.remove.as_ref()
    }

    /// Called by `main` on `Effect::OpenRegistryForm` with the registry's file values.
    pub fn open_registry_form(&mut self, original: Option<RegistryId>, form: RegistryForm) {
        self.form = Some(RegForm::new(original, form));
    }

    /// Called by `main` when the last `SaveRegistry` could not be written: the form comes back
    /// as it was saved, with `error`.
    pub fn registry_save_failed(&mut self, error: String) {
        if let Some(mut f) = self.form_saved.take() {
            f.error = Some(error);
            self.form = Some(f);
        } else {
            self.set_status(StatusKind::Error, error);
        }
    }

    /// `c` / `n` / `x` in Normal mode; they act only on the Registries pane.
    pub(super) fn registry_key(&mut self, a: Action) -> Vec<Effect> {
        if self.focus != Pane::Registries {
            return vec![];
        }
        let sel = self.selected_registry().map(|r| r.id.clone());
        match a {
            Action::RegistryEdit => sel
                .map(|id| Effect::OpenRegistryForm(Some(id)))
                .into_iter()
                .collect(),
            Action::RegistryNew => vec![Effect::OpenRegistryForm(None)],
            _ => {
                if self.registries.len() <= 1 {
                    self.set_status(StatusKind::Error, "cannot remove the only registry".into());
                } else if let Some(m) = self.reload_refusal() {
                    self.set_status(StatusKind::Error, m);
                } else {
                    self.remove = sel;
                }
                vec![]
            }
        }
    }

    /// The `x` prompt: `y` removes, `n` / Esc cancels.
    pub(super) fn remove_action(&mut self, a: Action) -> Vec<Effect> {
        match a {
            Action::Confirm(true) => self
                .remove
                .take()
                .map(Effect::RemoveRegistry)
                .into_iter()
                .collect(),
            Action::Confirm(false) => {
                self.remove = None;
                vec![]
            }
            Action::ForceQuit => vec![Effect::Quit],
            _ => vec![],
        }
    }

    pub(super) fn form_action(&mut self, a: Action) -> Vec<Effect> {
        if a == Action::ForceQuit {
            return vec![Effect::Quit];
        }
        if a == Action::FormSave {
            return self.form_save();
        }
        let confirming = self.form.as_ref().is_some_and(|f| f.confirm_discard);
        if a == Action::FormTest && !confirming {
            return self.form_test();
        }
        let Some(f) = self.form.as_mut() else {
            return vec![];
        };
        if f.confirm_discard {
            match a {
                Action::Confirm(true) => self.form = None,
                Action::Confirm(false) => f.confirm_discard = false,
                _ => {}
            }
            return vec![];
        }
        let before = f.values.clone();
        match a {
            Action::FormNext => f.step(true),
            Action::FormPrev => f.step(false),
            Action::Back if f.dirty() => f.confirm_discard = true,
            Action::Back => self.form = None,
            Action::Left | Action::Right => {
                let fwd = a == Action::Right;
                match f.focus {
                    Field::Type => f.values.kind = cycle(&KINDS, f.values.kind, fwd),
                    Field::Auth => f.values.auth = cycle(&AUTHS, f.values.auth, fwd),
                    _ => {}
                }
            }
            Action::FilterChar(c) => {
                if let Some(t) = f.text_mut(f.focus) {
                    t.push(c);
                }
            }
            Action::FilterBackspace => {
                if let Some(t) = f.text_mut(f.focus) {
                    t.pop();
                }
            }
            _ => {}
        }
        // A verdict is about the values it tested.
        if let Some(f) = self.form.as_mut()
            && f.values != before
        {
            f.test = None;
        }
        vec![]
    }

    /// Ids the form's id must not reuse: every registry but the one it edits.
    fn taken_ids(&self, f: &RegForm) -> Vec<RegistryId> {
        self.registries
            .iter()
            .map(|r| r.id.clone())
            .filter(|id| Some(id) != f.original.as_ref())
            .collect()
    }

    /// Ctrl-T: step 0 (`config valid`) here; a valid config runs the rest in the runtime.
    fn form_test(&mut self) -> Vec<Effect> {
        let Some(f) = self.form.as_ref() else {
            return vec![];
        };
        let valid = validate(&f.values, &self.taken_ids(f));
        self.next_op += 1;
        let op = OpId(self.next_op);
        let mut checks: [Option<Check>; 5] = Default::default();
        let (status, detail, fx) = match valid {
            Ok(cfg) => (
                CheckStatus::Ok,
                String::new(),
                vec![Effect::TestConnection { op, cfg }],
            ),
            Err(e) => {
                checks[1..].fill(Some(Check::skipped()));
                (CheckStatus::Failed, e, vec![])
            }
        };
        checks[0] = Some(Check {
            status,
            detail,
            ms: 0,
        });
        if let Some(f) = self.form.as_mut() {
            f.test = Some(ConnTest { op, checks });
            f.error = None;
        }
        fx
    }

    /// A step of the running test; any other op (an earlier test, a closed form) is ignored.
    pub(super) fn conn_check(&mut self, op: OpId, step: usize, check: Check) {
        if let Some(t) = self.form.as_mut().and_then(|f| f.test.as_mut())
            && t.op == op
            && step < STEPS.len()
        {
            t.checks[step] = Some(check);
        }
    }

    /// Ctrl-S: the reload guard, then validation; a valid form closes and is written by `main`.
    fn form_save(&mut self) -> Vec<Effect> {
        if let Some(m) = self.reload_refusal() {
            self.set_status(StatusKind::Error, m);
            return vec![];
        }
        let Some(f) = self.form.as_ref() else {
            return vec![];
        };
        let taken = self.taken_ids(f);
        let f = self.form.as_mut().expect("checked above");
        if let Err(e) = validate(&f.values, &taken) {
            f.error = Some(e);
            return vec![];
        }
        let f = self.form.take().expect("checked above");
        let fx = Effect::SaveRegistry {
            original: f.original.clone(),
            form: f.values.clone(),
        };
        self.form_saved = Some(f);
        vec![fx]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::delete::{DelStage, DeleteModal};
    use crate::app::state::{Action, Effect, Event, KeyMode, RegistryView, State};
    use crate::app::update::tests::{T, dg, ecr, rid, rp};
    use crate::app::write::{OpId, PullModal, PullStage, RELOAD_BUSY};
    use crate::config_edit::{AuthKind, Kind, RegistryForm};
    use crate::conntest::{Check, CheckStatus};
    use crate::domain::DeleteReport;
    use Field::*;

    fn harbor() -> RegistryView {
        RegistryView {
            id: rid("harbor"),
            provider: "oci",
            ..ecr()
        }
    }

    fn two() -> T {
        T::new(vec![ecr(), harbor()]).0
    }

    fn harbor_form() -> RegistryForm {
        RegistryForm {
            id: "harbor".into(),
            kind: Kind::Oci,
            endpoint: "https://harbor.example".into(),
            auth: AuthKind::Anonymous,
            ..RegistryForm::new_default()
        }
    }

    /// The form effects only (no fetches / focus).
    fn mine(fx: &[Effect]) -> Vec<Effect> {
        fx.iter()
            .filter(|e| {
                matches!(
                    e,
                    Effect::OpenRegistryForm(_)
                        | Effect::SaveRegistry { .. }
                        | Effect::RemoveRegistry(_)
                        | Effect::EditConfig
                )
            })
            .cloned()
            .collect()
    }

    fn form(t: &T) -> RegForm {
        t.s.reg_form().expect("form open").clone()
    }

    fn msg(s: &State) -> Option<&str> {
        s.status().map(|(_, m)| m)
    }

    fn typed(t: &mut T, s: &str) {
        for c in s.chars() {
            t.act(Action::FilterChar(c));
        }
    }

    fn focus_on(t: &mut T, f: Field) {
        for _ in 0..20 {
            if form(t).focus == f {
                return;
            }
            t.act(Action::FormNext);
        }
        panic!("no field {f:?}");
    }

    #[test]
    fn keys_open_form() {
        let mut t = two();
        t.act(Action::Down);
        assert_eq!(
            mine(&t.act(Action::RegistryEdit)),
            vec![Effect::OpenRegistryForm(Some(rid("harbor")))]
        );
        assert_eq!(
            mine(&t.act(Action::RegistryNew)),
            vec![Effect::OpenRegistryForm(None)]
        );
        t.act(Action::NextPane);
        for a in [
            Action::RegistryEdit,
            Action::RegistryNew,
            Action::RegistryRemove,
        ] {
            assert!(mine(&t.act(a)).is_empty(), "{a:?}");
            assert!(t.s.remove_confirm().is_none() && t.s.reg_form().is_none());
        }
    }

    #[test]
    fn form_navigation_and_typing() {
        let mut t = two();
        t.s.open_registry_form(Some(rid("harbor")), harbor_form());
        assert_eq!(t.s.key_mode(), KeyMode::Form);
        let oci = vec![
            Id,
            Type,
            Endpoint,
            RefHost,
            Auth,
            Repos,
            TrustedRealms,
            Pinned,
        ];
        assert_eq!(form(&t).fields(), oci);
        assert_eq!(form(&t).focus, Id);
        t.act(Action::FormNext);
        assert_eq!(form(&t).focus, Type);
        t.act(Action::FormPrev);
        assert_eq!(form(&t).focus, Id);
        t.act(Action::FormPrev);
        assert_eq!(form(&t).focus, Pinned, "wraps");
        t.act(Action::FormNext);
        assert_eq!(form(&t).focus, Id);

        typed(&mut t, "2x");
        t.act(Action::FilterBackspace);
        assert_eq!(form(&t).values.id, "harbor2");
        t.act(Action::Right);
        assert_eq!(form(&t).values.id, "harbor2", "←→ do nothing on text");

        focus_on(&mut t, Type);
        typed(&mut t, "z");
        assert_eq!(form(&t).values.kind, Kind::Oci, "typing on Type is inert");
        t.act(Action::Right);
        assert_eq!(form(&t).values.kind, Kind::Ecr);
        assert_eq!(
            form(&t).fields(),
            vec![
                Id, Type, Account, Region, Profile, Endpoint, RefHost, Pinned
            ]
        );
        assert_eq!(form(&t).focus, Type);
        t.act(Action::Right);
        assert_eq!(form(&t).values.kind, Kind::EcrPublic);
        assert_eq!(
            form(&t).fields(),
            vec![Id, Type, Alias, Profile, Endpoint, RefHost, Pinned]
        );
        t.act(Action::Left);
        t.act(Action::Left);
        assert_eq!(form(&t).values.kind, Kind::Oci);

        focus_on(&mut t, Auth);
        t.act(Action::Right);
        assert_eq!(form(&t).values.auth, AuthKind::DockerConfig);
        t.act(Action::Right);
        assert_eq!(form(&t).values.auth, AuthKind::Basic);
        let basic = vec![
            Id,
            Type,
            Endpoint,
            RefHost,
            Auth,
            Username,
            PasswordEnv,
            Repos,
            TrustedRealms,
            Pinned,
        ];
        assert_eq!(form(&t).fields(), basic);
        assert_eq!(form(&t).focus, Auth);
        t.act(Action::Right);
        assert_eq!(form(&t).values.auth, AuthKind::Anonymous, "wraps");
        t.act(Action::Left);
        assert_eq!(form(&t).values.auth, AuthKind::Basic);
        focus_on(&mut t, PasswordEnv);
        typed(&mut t, "TOKEN");
        assert_eq!(form(&t).values.password_env, "TOKEN");
    }

    #[test]
    fn dirty_esc_asks_discard() {
        let mut t = two();
        t.s.open_registry_form(Some(rid("harbor")), harbor_form());
        assert!(!form(&t).dirty());
        assert!(mine(&t.act(Action::Back)).is_empty());
        assert!(t.s.reg_form().is_none(), "clean form closes at once");
        assert_eq!(t.s.key_mode(), KeyMode::Normal);

        t.s.open_registry_form(Some(rid("harbor")), harbor_form());
        typed(&mut t, "x");
        assert!(form(&t).dirty());
        t.act(Action::Back);
        assert!(form(&t).confirm_discard);
        assert_eq!(t.s.key_mode(), KeyMode::Confirm);
        // `n` and Esc both map to Confirm(false).
        t.act(Action::Confirm(false));
        assert!(!form(&t).confirm_discard);
        assert_eq!(form(&t).values.id, "harborx", "edits kept");
        assert_eq!(t.s.key_mode(), KeyMode::Form);
        t.act(Action::Back);
        assert!(mine(&t.act(Action::Confirm(true))).is_empty());
        assert!(t.s.reg_form().is_none());
        assert_eq!(t.s.key_mode(), KeyMode::Normal);
    }

    #[test]
    fn save_validates_then_emits() {
        let mut t = two();
        t.s.open_registry_form(None, RegistryForm::new_default());
        assert!(mine(&t.act(Action::FormSave)).is_empty());
        assert_eq!(
            form(&t).error.as_deref(),
            Some("field `id` must not be empty")
        );
        typed(&mut t, "ghcr");
        focus_on(&mut t, Endpoint);
        typed(&mut t, "https://ghcr.io");
        let values = form(&t).values;
        assert_eq!(
            mine(&t.act(Action::FormSave)),
            vec![Effect::SaveRegistry {
                original: None,
                form: values
            }]
        );
        assert!(t.s.reg_form().is_none());
        assert_eq!(t.s.key_mode(), KeyMode::Normal);
    }

    #[test]
    fn failed_write_reopens_the_form_dirty() {
        let mut t = two();
        t.s.open_registry_form(Some(rid("harbor")), harbor_form());
        typed(&mut t, "2");
        t.act(Action::FormSave);
        assert!(t.s.reg_form().is_none());
        t.s.registry_save_failed("registries is an inline array".into());
        let f = form(&t);
        assert_eq!(f.values.id, "harbor2");
        assert_eq!(f.initial, harbor_form());
        assert!(f.dirty());
        assert_eq!(f.error.as_deref(), Some("registries is an inline array"));
    }

    #[test]
    fn remove_confirm() {
        let mut t = two();
        assert!(mine(&t.act(Action::RegistryRemove)).is_empty());
        assert_eq!(t.s.remove_confirm(), Some(&rid("ecr")));
        assert_eq!(t.s.key_mode(), KeyMode::Confirm);
        assert!(mine(&t.act(Action::Confirm(false))).is_empty());
        assert_eq!(t.s.remove_confirm(), None);
        t.act(Action::RegistryRemove);
        assert_eq!(
            mine(&t.act(Action::Confirm(true))),
            vec![Effect::RemoveRegistry(rid("ecr"))]
        );
        assert_eq!(t.s.remove_confirm(), None);

        let (mut one, _) = T::new(vec![ecr()]);
        one.act(Action::RegistryRemove);
        assert_eq!(one.s.remove_confirm(), None);
        assert_eq!(msg(&one.s), Some("cannot remove the only registry"));
    }

    #[test]
    fn save_keeps_own_id() {
        let mut t = two();
        t.s.open_registry_form(Some(rid("harbor")), harbor_form());
        typed(&mut t, " ");
        assert!(matches!(
            mine(&t.act(Action::FormSave))[..],
            [Effect::SaveRegistry { .. }]
        ));
        t.s.open_registry_form(Some(rid("harbor")), harbor_form());
        for _ in 0.."harbor".len() {
            t.act(Action::FilterBackspace);
        }
        typed(&mut t, "ecr");
        assert!(mine(&t.act(Action::FormSave)).is_empty());
        assert_eq!(form(&t).error.as_deref(), Some("duplicate id"));
    }

    #[test]
    fn reload_guard_order() {
        let push = "another push/delete/copy is running";
        let report = "close the delete report first (press d)";
        for (name, expected) in [("push", push), ("report", report), ("pull", RELOAD_BUSY)] {
            let mut t = two();
            // Each later case drops the earlier, higher-priority refusal.
            if name == "push" {
                t.s.start_push(rid("ecr"), rp("app"), "img".into(), "1".into())
                    .unwrap();
            }
            if name != "pull" {
                t.s.delete = Some(DeleteModal {
                    registry: rid("ecr"),
                    repo: rp("app"),
                    op: OpId(1),
                    stage: DelStage::Report {
                        report: DeleteReport {
                            results: vec![],
                            aborted_unsupported: false,
                        },
                        stopped: None,
                        refreshed: false,
                    },
                    whole: false,
                    typed: String::new(),
                    snapshot: None,
                    plan: None,
                    changed: false,
                    scroll: 0,
                    hidden: true,
                });
            }
            t.s.pull = Some(PullModal {
                registry: rid("ecr"),
                repo: rp("app"),
                digest: dg('a'),
                image: "h.example/app:1".into(),
                child: None,
                platform: None,
                stage: PullStage::Running,
                op: Some(OpId(2)),
                transfer: None,
                hidden: true,
            });

            assert!(mine(&t.act(Action::EditConfig)).is_empty(), "{name}: e");
            assert_eq!(msg(&t.s), Some(expected), "{name}: e");

            t.s.status = None;
            t.s.open_registry_form(Some(rid("harbor")), harbor_form());
            assert!(mine(&t.act(Action::FormSave)).is_empty(), "{name}: save");
            assert_eq!(msg(&t.s), Some(expected), "{name}: save");
            assert!(t.s.reg_form().is_some(), "{name}: form stays open");
            t.act(Action::Back);

            t.s.status = None;
            t.act(Action::RegistryRemove);
            assert!(mine(&t.act(Action::Confirm(true))).is_empty(), "{name}: x");
            assert_eq!(msg(&t.s), Some(expected), "{name}: x");
            assert_eq!(t.s.remove_confirm(), None, "{name}: x");
        }
    }

    fn check(status: CheckStatus, detail: &str) -> Check {
        Check {
            status,
            detail: detail.into(),
            ms: 0,
        }
    }

    fn conn(fx: &[Effect]) -> Vec<Effect> {
        fx.iter()
            .filter(|e| matches!(e, Effect::TestConnection { .. }))
            .cloned()
            .collect()
    }

    fn test_of(t: &T) -> ConnTest {
        form(t).test.expect("test started")
    }

    #[test]
    fn ctrl_t_runs_step1_in_app() {
        let mut t = two();
        t.s.open_registry_form(None, RegistryForm::new_default());
        assert!(conn(&t.act(Action::FormTest)).is_empty());
        let c = test_of(&t);
        assert_eq!(
            c.checks[0],
            Some(check(CheckStatus::Failed, "field `id` must not be empty"))
        );
        for s in &c.checks[1..] {
            assert_eq!(s.as_ref().map(|c| &c.status), Some(&CheckStatus::Skipped));
        }
        assert_eq!(c.verdict(), Some(false));

        t.s.open_registry_form(Some(rid("harbor")), harbor_form());
        let fx = conn(&t.act(Action::FormTest));
        let c = test_of(&t);
        let cfg = validate(&harbor_form(), &[rid("ecr")]).unwrap();
        assert_eq!(fx, vec![Effect::TestConnection { op: c.op, cfg }]);
        assert_eq!(c.checks[0], Some(check(CheckStatus::Ok, "")));
        assert!(c.checks[1..].iter().all(Option::is_none));
        assert_eq!(c.verdict(), None, "running");

        let ok = check(CheckStatus::Ok, "none needed");
        t.ev(Event::ConnCheck {
            op: OpId(c.op.0 + 100),
            step: 1,
            check: ok.clone(),
        });
        assert_eq!(test_of(&t).checks[1], None, "stale op ignored");
        t.ev(Event::ConnCheck {
            op: c.op,
            step: 1,
            check: ok.clone(),
        });
        assert_eq!(test_of(&t).checks[1], Some(ok));

        // A second Ctrl-T starts over under a new op.
        t.act(Action::FormTest);
        assert_ne!(test_of(&t).op, c.op);
        assert_eq!(test_of(&t).checks[1], None);
    }

    #[test]
    fn verdict_requires_all() {
        let ok = check(CheckStatus::Ok, "");
        let mut c = ConnTest {
            op: OpId(1),
            checks: std::array::from_fn(|_| Some(ok.clone())),
        };
        assert_eq!(c.verdict(), Some(true));
        assert_eq!(c.verdict_text(), Some("Ready"));
        c.checks[4] = Some(check(CheckStatus::Skipped, ""));
        assert_eq!(c.verdict(), Some(false));
        assert_eq!(
            c.verdict_text(),
            Some("Not working — save anyway with Ctrl-S")
        );
        c.checks[4] = None;
        assert_eq!(c.verdict(), None);
        assert_eq!(c.verdict_text(), None);
    }

    #[test]
    fn edit_clears_stale_verdict() {
        let ready = |t: &mut T| {
            t.act(Action::FormTest);
            let op = test_of(t).op;
            for step in 1..5 {
                t.ev(Event::ConnCheck {
                    op,
                    step,
                    check: check(CheckStatus::Ok, ""),
                });
            }
            assert_eq!(test_of(t).verdict_text(), Some("Ready"));
        };
        let mut t = two();
        t.s.open_registry_form(Some(rid("harbor")), harbor_form());
        ready(&mut t);
        focus_on(&mut t, Endpoint);
        typed(&mut t, "x");
        assert_eq!(form(&t).test, None, "typing");
        ready(&mut t);
        t.act(Action::FilterBackspace);
        assert_eq!(form(&t).test, None, "backspace");
        ready(&mut t);
        focus_on(&mut t, Auth);
        assert!(form(&t).test.is_some(), "moving the focus keeps it");
        t.act(Action::Right);
        assert_eq!(form(&t).test, None, "auth cycle");
        focus_on(&mut t, Type);
        ready(&mut t);
        t.act(Action::Left);
        assert_eq!(form(&t).test, None, "type cycle");

        // Ctrl-T clears an earlier save error.
        t.s.form.as_mut().unwrap().error = Some("bad".into());
        t.act(Action::FormTest);
        assert_eq!(form(&t).error, None);
    }

    #[test]
    fn save_after_failed_test_allowed() {
        let mut t = two();
        t.s.open_registry_form(Some(rid("harbor")), harbor_form());
        t.act(Action::FormTest);
        let op = test_of(&t).op;
        t.ev(Event::ConnCheck {
            op,
            step: 1,
            check: check(CheckStatus::Failed, "$X not set"),
        });
        for step in 2..5 {
            t.ev(Event::ConnCheck {
                op,
                step,
                check: check(CheckStatus::Skipped, ""),
            });
        }
        assert_eq!(test_of(&t).verdict(), Some(false));
        assert_eq!(
            mine(&t.act(Action::FormSave)),
            vec![Effect::SaveRegistry {
                original: Some(rid("harbor")),
                form: harbor_form()
            }]
        );
    }

    #[test]
    fn labels_and_hints() {
        assert_eq!(RegForm::label(Endpoint, Kind::Oci), "Endpoint");
        assert_eq!(RegForm::label(Endpoint, Kind::Ecr), "Endpoint override");
        assert_eq!(RegForm::label(RefHost, Kind::Oci), "Ref host override");
        assert_eq!(RegForm::label(PasswordEnv, Kind::Oci), "Password env");
        assert_eq!(RegForm::label(TrustedRealms, Kind::Oci), "Trusted realms");
        let mut f = RegForm::new(None, RegistryForm::new_default());
        assert_eq!(
            f.hint(Repos).as_deref(),
            Some("needed when the registry has no catalog")
        );
        assert_eq!(f.hint(Profile).as_deref(), Some("empty = default chain"));
        assert_eq!(f.hint(Endpoint), None, "oci endpoint is required");
        f.values.kind = Kind::Ecr;
        f.values.account_id = "123456789012".into();
        f.values.region = "us-west-2".into();
        assert_eq!(
            f.hint(Endpoint).as_deref(),
            Some("https://123456789012.dkr.ecr.us-west-2.amazonaws.com (auto)")
        );
        f.values.kind = Kind::EcrPublic;
        assert_eq!(
            f.hint(Endpoint).as_deref(),
            Some("https://public.ecr.aws (auto)")
        );
        f.values.endpoint = "https://mirror.example".into();
        assert_eq!(f.hint(Endpoint), None, "an override replaces the hint");
    }
}
