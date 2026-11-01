//! lcquay binary: config, logging, terminal guard and the event loop. Logic lives in the lib.

use std::future::Future;
use std::io::{Stdout, stdout};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use crossterm::cursor::{Hide, Show};
use crossterm::event::{Event as TermEvent, EventStream};
use crossterm::execute;
use crossterm::terminal::{
    Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use time::OffsetDateTime;
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;

use lcquay::app::state::{Action, Effect, Event, State, StatusKind};
use lcquay::app::update::{Input, update};
use lcquay::config::{self, Config};
use lcquay::config_edit::{self, RegistryForm};
use lcquay::docker::{BollardEngine, Engine};
use lcquay::domain::{RegistryError, RegistryId};
use lcquay::keys::key_to_action;
use lcquay::providers::build_registries;
use lcquay::reqlog::RequestLog;
use lcquay::tasks::{ArboardClipboard, Runtime, view};
use lcquay::ui::{ViewState, render};

/// Restores the terminal on every exit path, including unwinding.
struct TerminalGuard;

impl TerminalGuard {
    fn new() -> std::io::Result<Self> {
        enable_raw_mode()?;
        let guard = TerminalGuard; // from here on Drop undoes whatever was done
        execute!(stdout(), EnterAlternateScreen, Hide)?;
        Ok(guard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(stdout(), Show, LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

/// Filter directives and an optional warning. `LCQUAY_LOG` (one level name) only controls `lcquay`;
/// every other target is fixed at `warn`, because debug logs of the HTTP/AWS crates contain full
/// URLs with signed query strings and no directive list of them could be complete.
fn log_directives(env: Option<&str>) -> (String, Option<String>) {
    let (level, warning) = match env.map(|v| v.trim().to_ascii_lowercase()) {
        None => ("info".to_owned(), None),
        Some(v) if ["error", "warn", "info", "debug", "trace", "off"].contains(&v.as_str()) => {
            (v, None)
        }
        Some(v) => (
            "info".to_owned(),
            Some(format!(
                "invalid LCQUAY_LOG {v:?} (expected one of error, warn, info, debug, trace, off); using info"
            )),
        ),
    };
    (format!("warn,lcquay={level}"), warning)
}

/// Returns the appender guard, which must live as long as the program.
fn init_logging() -> anyhow::Result<tracing_appender::non_blocking::WorkerGuard> {
    let path = config::log_path();
    let dir = path.parent().unwrap_or_else(|| ".".as_ref());
    std::fs::create_dir_all(dir)?;
    let file = path.file_name().unwrap_or_else(|| "lcquay.log".as_ref());
    let (writer, guard) =
        tracing_appender::non_blocking(tracing_appender::rolling::never(dir, file));
    let (directives, warning) = log_directives(std::env::var("LCQUAY_LOG").ok().as_deref());
    if let Some(w) = warning {
        eprintln!("lcquay: {w}");
    }
    tracing_subscriber::fmt()
        .with_writer(writer)
        .with_ansi(false)
        .with_env_filter(EnvFilter::new(directives))
        .init();
    // Never touches the terminal: the guard owns restoration.
    std::panic::set_hook(Box::new(|info| {
        let location = info.location().map(ToString::to_string);
        tracing::error!(
            message = info
                .payload_as_str()
                .unwrap_or("<non-string panic payload>"),
            location = location.as_deref().unwrap_or("<unknown>"),
            "panic"
        );
    }));
    Ok(guard)
}

/// What the event loop does after `apply`.
#[derive(PartialEq)]
enum Next {
    Continue,
    Quit,
    EditConfig,
    /// The config file was rewritten with this text: reload it.
    Reload(String),
}

/// Applies `input`, runs the resulting effects except the ones `event_loop` handles. The
/// registry form's file reads and writes happen here (sync); the reload is `event_loop`'s.
fn apply(state: &mut State, rt: &Runtime, input: Input, path: &Path) -> Next {
    let mut next = Next::Continue;
    for fx in update(state, input, OffsetDateTime::now_utc()) {
        match fx {
            Effect::Quit => next = Next::Quit,
            Effect::EditConfig if next == Next::Continue => next = Next::EditConfig,
            Effect::SaveTheme(t) => {
                if let Err(e) = config::save_theme(&config::theme_path(), t) {
                    state.set_status(StatusKind::Error, format!("theme not saved: {e}"));
                }
            }
            Effect::OpenRegistryForm(original) => open_registry_form(state, path, original),
            Effect::SaveRegistry { original, form } => {
                match read_config(path)
                    .and_then(|text| save_registry_text(&text, original.as_ref(), &form, path))
                {
                    Ok(text) if next == Next::Continue => next = Next::Reload(text),
                    Ok(_) => {}
                    Err(e) => state.registry_save_failed(e),
                }
            }
            Effect::RemoveRegistry(id) => {
                match read_config(path).and_then(|text| remove_registry_text(&text, &id, path)) {
                    Ok(text) if next == Next::Continue => next = Next::Reload(text),
                    Ok(_) => {}
                    Err(e) => state.set_status(StatusKind::Error, format!("{id} not removed: {e}")),
                }
            }
            fx => rt.run(fx),
        }
    }
    next
}

fn read_config(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))
}

/// `Effect::OpenRegistryForm`: the registry's file values (a fresh form for a new one).
fn open_registry_form(state: &mut State, path: &Path, original: Option<RegistryId>) {
    let form = match &original {
        None => Ok(RegistryForm::new_default()),
        Some(id) => read_config(path).and_then(|text| config_edit::read_form(&text, id)),
    };
    match form {
        Ok(form) => state.open_registry_form(original, form),
        Err(e) => state.set_status(StatusKind::Error, format!("cannot open the form: {e}")),
    }
}

/// Writes `new` to `path` only if the loader accepts it; returns it.
fn write_checked(new: String, path: &Path) -> Result<String, String> {
    config::parse(&new, path).map_err(|e| e.to_string())?;
    config::write_atomic(path, &new).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(new)
}

/// The config `text` with `form` saved into it (`original`: the registry it edits), written.
fn save_registry_text(
    text: &str,
    original: Option<&RegistryId>,
    form: &RegistryForm,
    path: &Path,
) -> Result<String, String> {
    write_checked(config_edit::upsert(text, original, form)?, path)
}

/// The config `text` without registry `id`, written.
fn remove_registry_text(text: &str, id: &RegistryId, path: &Path) -> Result<String, String> {
    write_checked(config_edit::remove(text, id)?, path)
}

/// Parses `text` and starts a fresh `State` and `Runtime` on it, keeping the request log. Shared
/// by `e` and the registry form.
async fn reload(
    text: &str,
    path: &Path,
    local: Option<Arc<dyn Engine>>,
    log: &RequestLog,
) -> Result<(State, Runtime, mpsc::UnboundedReceiver<Event>), String> {
    let cfg = config::parse(text, path).map_err(|e| e.to_string())?;
    start(&cfg, local, log).await.map_err(|e| e.to_string())
}

/// Builds the registries of `cfg`, a fresh `State` and the `Runtime` that ran its initial effects.
async fn start(
    cfg: &Config,
    local: Option<Arc<dyn Engine>>,
    log: &RequestLog,
) -> Result<(State, Runtime, mpsc::UnboundedReceiver<Event>), RegistryError> {
    let registries = build_registries(cfg, log).await?;
    let (mut state, initial) = State::new(registries.iter().map(view).collect());
    state.theme = config::load_theme(&config::theme_path());
    let (tx, rx) = mpsc::unbounded_channel();
    let rt = Runtime::new(registries, local, Box::new(ArboardClipboard::default()), tx)
        .with_log(log.clone());
    for fx in initial {
        rt.run(fx);
    }
    Ok((state, rt, rx))
}

/// Program and args from `$VISUAL`, else `$EDITOR`, else `vi`; blank values count as unset.
fn editor_command(visual: Option<&str>, editor: Option<&str>) -> (String, Vec<String>) {
    let cmd = [visual, editor]
        .into_iter()
        .flatten()
        .find(|v| !v.trim().is_empty())
        .unwrap_or("vi");
    // ponytail: whitespace split, no shell quoting; editor paths with spaces are unsupported.
    let mut words = cmd.split_whitespace().map(str::to_owned);
    let program = words.next().unwrap_or_default(); // `cmd` is not blank
    (program, words.collect())
}

/// Runs the editor on `path` (terminal suspended); `Ok(Some(text))` if the file changed.
async fn run_editor(path: &Path) -> Result<Option<String>, String> {
    let read = || std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()));
    let before = read()?;
    // Raw mode is off, so Ctrl-C / Ctrl-\ signal our process group too: catch them so only the
    // editor stops (like git). The handler stays installed; raw mode keeps them keys afterwards.
    #[cfg(unix)]
    let _signals = {
        use tokio::signal::unix::{SignalKind, signal};
        (signal(SignalKind::interrupt()), signal(SignalKind::quit()))
    };
    let (program, args) = editor_command(
        std::env::var("VISUAL").ok().as_deref(),
        std::env::var("EDITOR").ok().as_deref(),
    );
    let status = tokio::process::Command::new(&program)
        .args(&args)
        .arg(path)
        .status()
        .await
        .map_err(|e| format!("editor {program} failed to start: {e}"))?;
    if !status.success() {
        return Err(format!("editor exited with {status}; config not reloaded"));
    }
    let after = read()?;
    Ok((after != before).then_some(after))
}

/// Copies the log into the overlay when it is open and the log changed since `seen`; `true` if it
/// did. While the overlay is closed `seen` is reset, so the next opening copies at once.
fn refresh_requests(state: &mut State, log: &RequestLog, seen: &mut u64) -> bool {
    if !state.requests_open() {
        *seen = u64::MAX;
        return false;
    }
    let generation = log.generation();
    if generation == *seen {
        return false;
    }
    *seen = generation;
    state.set_requests(log.snapshot());
    true
}

async fn event_loop(
    mut state: State,
    mut rt: Runtime,
    mut rx: mpsc::UnboundedReceiver<Event>,
    path: PathBuf,
    local: Option<Arc<dyn Engine>>,
    requests: RequestLog,
) -> anyhow::Result<()> {
    let mut terminal = Terminal::new(CrosstermBackend::<Stdout>::new(stdout()))?;
    let mut keys = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(33));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut view = ViewState::default();
    let mut dirty = true;
    // Generation of the log last copied into the requests overlay.
    let mut requests_seen = u64::MAX;
    // Redraw each minute so the login time left stays current.
    let mut drawn_minute = None;
    loop {
        let next = tokio::select! {
            ev = keys.next() => match ev {
                Some(Ok(TermEvent::Key(k))) => match key_to_action(k, state.key_mode()) {
                    // Scrolling past the end of the detail pane or the delete modal is
                    // dropped, not stored.
                    Some(a) if !view.past_end(&state, a) => {
                        dirty = true;
                        apply(&mut state, &rt, Input::Action(a), &path)
                    }
                    _ => Next::Continue,
                },
                Some(Ok(TermEvent::Resize(..))) => {
                    dirty = true;
                    Next::Continue
                }
                Some(Ok(_)) => Next::Continue,
                Some(Err(e)) => return Err(e.into()),
                None => anyhow::bail!("terminal input closed"),
            },
            ev = rx.recv() => {
                let Some(ev) = ev else { anyhow::bail!("event channel closed unexpectedly") };
                dirty = true;
                apply(&mut state, &rt, Input::Event(ev), &path)
            }
            _ = tick.tick() => {
                let now = OffsetDateTime::now_utc();
                dirty |= refresh_requests(&mut state, &requests, &mut requests_seen);
                if drawn_minute != Some(now.minute()) {
                    dirty = true;
                }
                if dirty {
                    terminal.draw(|f| render(f, &state, now, &mut view))?;
                    drawn_minute = Some(now.minute());
                    dirty = false;
                    // Over-scroll (e.g. after a resize): pull it back so one Ctrl-u moves the view.
                    if state.scroll() > view.detail_max_scroll {
                        apply(&mut state, &rt, Input::Action(Action::ClampScroll(view.detail_max_scroll)), &path);
                        dirty = true;
                    }
                }
                Next::Continue
            }
        };
        match next {
            Next::Continue => {}
            Next::Quit => return Ok(()),
            Next::EditConfig => {
                // The stream's reader would steal the editor's keystrokes.
                drop(keys);
                execute!(stdout(), LeaveAlternateScreen, Show)?;
                disable_raw_mode()?;
                let edited = run_editor(&path).await;
                enable_raw_mode()?;
                execute!(stdout(), EnterAlternateScreen, Hide, Clear(ClearType::All))?;
                // Not `terminal.clear()`: it queries the cursor position, and that poll eats the
                // wake left by dropping `keys` and fails. A fresh fullscreen terminal never
                // queries; the new stream treats the stale wake as "no event yet".
                terminal = Terminal::new(CrosstermBackend::new(stdout()))?;
                keys = EventStream::new();
                dirty = true;
                let (kind, msg) = match edited {
                    Err(msg) => (StatusKind::Error, msg),
                    Ok(None) => (StatusKind::Info, "config unchanged".to_owned()),
                    Ok(Some(text)) => match reload(&text, &path, local.clone(), &requests).await {
                        Err(e) => (StatusKind::Error, e),
                        // Dropping the old receiver makes the old tasks' sends fail harmlessly.
                        Ok(fresh) => {
                            (state, rt, rx) = fresh;
                            (StatusKind::Ok, "config reloaded".to_owned())
                        }
                    },
                };
                state.set_status(kind, msg);
            }
            Next::Reload(text) => {
                dirty = true;
                let (kind, msg) = match reload(&text, &path, local.clone(), &requests).await {
                    Err(e) => (
                        StatusKind::Error,
                        format!("config saved; reload failed: {e}"),
                    ),
                    Ok(fresh) => {
                        (state, rt, rx) = fresh;
                        (StatusKind::Ok, "config saved and reloaded".to_owned())
                    }
                };
                state.set_status(kind, msg);
            }
        }
    }
}

async fn run() -> anyhow::Result<ExitCode> {
    let path = config::config_path();
    let cfg = match config::load_or_write_template(&path) {
        Ok(Some(cfg)) => cfg,
        Ok(None) => {
            println!(
                "wrote config template to {}; edit it and run lcquay again",
                path.display()
            );
            return Ok(ExitCode::SUCCESS);
        }
        Err(e) => {
            eprintln!("lcquay: {e}");
            return Ok(ExitCode::from(2));
        }
    };
    let _log = init_logging()?;
    let local: Option<Arc<dyn Engine>> = match BollardEngine::connect() {
        Ok(e) => Some(Arc::new(e)),
        Err(e) => {
            tracing::warn!("docker unavailable: {e}");
            None
        }
    };
    // One log for the whole session: a config reload keeps the entries.
    let requests = RequestLog::default();
    let (state, rt, rx) = match start(&cfg, local.clone(), &requests).await {
        Ok(started) => started,
        Err(e) => {
            eprintln!("lcquay: {e}");
            return Ok(ExitCode::from(2));
        }
    };
    // The guard lives inside the task, so it drops (terminal restored) while a panic unwinds;
    // the message is printed only afterwards.
    let ui = async move {
        let _terminal = TerminalGuard::new()?;
        event_loop(state, rt, rx, path, local, requests).await
    };
    match supervise(ui).await {
        Ok(()) => Ok(ExitCode::SUCCESS),
        Err(Failure::Error(e)) => Err(e),
        Err(Failure::Panic(msg)) => {
            eprintln!(
                "lcquay crashed: {msg} (details in {})",
                config::log_path().display()
            );
            Ok(ExitCode::from(101))
        }
    }
}

enum Failure {
    Error(anyhow::Error),
    Panic(String),
}

/// Runs `fut` as a task so a panic unwinds inside it and is recovered here as a message.
async fn supervise(
    fut: impl Future<Output = anyhow::Result<()>> + Send + 'static,
) -> Result<(), Failure> {
    match tokio::spawn(fut).await {
        Ok(r) => r.map_err(Failure::Error),
        Err(e) => match e.try_into_panic() {
            Ok(p) => Err(Failure::Panic(
                p.downcast_ref::<&str>()
                    .map(|s| (*s).to_owned())
                    .or_else(|| p.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "<non-string panic payload>".to_owned()),
            )),
            Err(e) => Err(Failure::Error(e.into())),
        },
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    run().await.unwrap_or_else(|e| {
        eprintln!("lcquay: {e:#}");
        ExitCode::FAILURE
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_directives_cases() {
        assert_eq!(log_directives(None), ("warn,lcquay=info".into(), None));
        assert_eq!(
            log_directives(Some("debug")),
            ("warn,lcquay=debug".into(), None)
        );
        assert_eq!(
            log_directives(Some(" TRACE ")),
            ("warn,lcquay=trace".into(), None)
        );
        // A directive list from the user must not widen other targets.
        let (d, w) = log_directives(Some("reqwest=debug"));
        assert_eq!(d, "warn,lcquay=info");
        assert!(w.is_some_and(|w| w.contains("invalid LCQUAY_LOG")));
        assert_eq!(log_directives(Some("")).0, "warn,lcquay=info");
    }

    #[test]
    fn requests_refresh_only_when_changed() {
        use lcquay::reqlog::RequestEntry;
        let entry = || RequestEntry {
            at: OffsetDateTime::UNIX_EPOCH,
            registry: "ecr".into(),
            op: "GET".into(),
            path: "/v2/".into(),
            outcome: "200".into(),
            ms: 1,
            bytes: None,
        };
        let log = RequestLog::default();
        let (mut state, _) = State::new(vec![]);
        let mut seen = u64::MAX;
        log.push(entry());
        assert!(!refresh_requests(&mut state, &log, &mut seen), "closed");
        let _ = update(
            &mut state,
            Input::Action(Action::Requests),
            OffsetDateTime::UNIX_EPOCH,
        );
        assert!(
            refresh_requests(&mut state, &log, &mut seen),
            "opened: copies"
        );
        assert_eq!(state.requests().unwrap().entries.len(), 1);
        assert!(!refresh_requests(&mut state, &log, &mut seen), "unchanged");
        log.push(entry());
        assert!(refresh_requests(&mut state, &log, &mut seen), "changed");
        assert_eq!(state.requests().unwrap().entries.len(), 2);
        // Closing and reopening copies again, even with no new entry.
        let _ = update(
            &mut state,
            Input::Action(Action::Back),
            OffsetDateTime::UNIX_EPOCH,
        );
        assert!(
            !refresh_requests(&mut state, &log, &mut seen),
            "closed again"
        );
        let _ = update(
            &mut state,
            Input::Action(Action::Requests),
            OffsetDateTime::UNIX_EPOCH,
        );
        assert!(refresh_requests(&mut state, &log, &mut seen));
        assert_eq!(state.requests().unwrap().entries.len(), 2);
    }

    #[test]
    fn editor_command_cases() {
        let v = |p: &str, a: &[&str]| (p.to_owned(), a.iter().map(|s| s.to_string()).collect());
        assert_eq!(editor_command(Some("nvim"), Some("nano")), v("nvim", &[]));
        assert_eq!(
            editor_command(Some("code --wait"), None),
            v("code", &["--wait"])
        );
        assert_eq!(editor_command(Some(""), Some("nano")), v("nano", &[]));
        assert_eq!(editor_command(Some("  "), Some(" hx ")), v("hx", &[]));
        assert_eq!(editor_command(None, Some("\t")), v("vi", &[]));
        assert_eq!(editor_command(None, None), v("vi", &[]));
    }

    #[test]
    fn apply_form_effects() {
        use lcquay::config_edit::{AuthKind, Kind, RegistryForm};
        use lcquay::domain::RegistryId;
        const FILE: &str = "# mine\nschema_version = 1\n\n[[registries]]\nid = \"hub\" # keep\nendpoint = \"https://hub.example\"\nprovider = { type = \"oci\" }\nauth = { type = \"anonymous\" }\n";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, FILE).unwrap();
        let ghcr = RegistryForm {
            id: "ghcr".into(),
            kind: Kind::Oci,
            endpoint: "https://ghcr.io".into(),
            auth: AuthKind::DockerConfig,
            ..RegistryForm::new_default()
        };
        let text = save_registry_text(FILE, None, &ghcr, &path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text, "written");
        assert!(text.starts_with("# mine\n") && text.contains("id = \"hub\" # keep"));
        let ids: Vec<String> = config::parse(&text, &path)
            .unwrap()
            .registries
            .iter()
            .map(|r| r.id.0.clone())
            .collect();
        assert_eq!(ids, ["hub", "ghcr"]);

        // Removing goes through the same check-then-write path.
        let text = remove_registry_text(&text, &RegistryId("hub".into()), &path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        assert_eq!(config::parse(&text, &path).unwrap().registries.len(), 1);

        // An upsert result the loader rejects is reported, and the file is left alone.
        let bad = format!(
            "{FILE}\n[[registries]]\nid = \"ftp\"\nendpoint = \"ftp://x\"\nprovider = {{ type = \"oci\" }}\nauth = {{ type = \"anonymous\" }}\n"
        );
        std::fs::write(&path, &bad).unwrap();
        let err = save_registry_text(&bad, None, &ghcr, &path).unwrap_err();
        assert!(err.contains("scheme must be http or https"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), bad, "not written");

        // Upsert errors come back as they are (shown in the form).
        let inline = "schema_version = 1\nregistries = [{ id = \"a\", endpoint = \"https://a\", provider = { type = \"oci\" }, auth = { type = \"anonymous\" } }]\n";
        std::fs::write(&path, inline).unwrap();
        let err = save_registry_text(inline, None, &ghcr, &path).unwrap_err();
        assert!(err.contains("inline array"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), inline);

        // Opening: the registry's file values, a fresh form for `n`, a status on a read error.
        std::fs::write(&path, FILE).unwrap();
        let (mut state, _) = State::new(vec![]);
        open_registry_form(&mut state, &path, Some(RegistryId("hub".into())));
        let f = state.reg_form().unwrap();
        assert_eq!(f.values.endpoint, "https://hub.example");
        assert_eq!(f.original, Some(RegistryId("hub".into())));
        let (mut state, _) = State::new(vec![]);
        open_registry_form(&mut state, &path, None);
        assert_eq!(
            state.reg_form().unwrap().values,
            RegistryForm::new_default()
        );
        let (mut state, _) = State::new(vec![]);
        open_registry_form(
            &mut state,
            &dir.path().join("nope.toml"),
            Some(RegistryId("hub".into())),
        );
        assert!(state.reg_form().is_none());
        assert!(matches!(state.status(), Some((StatusKind::Error, m)) if m.contains("nope.toml")));
    }

    #[tokio::test]
    async fn supervise_recovers_panic_message() {
        let r = supervise(async { panic!("boom {}", 1) }).await;
        assert!(matches!(r, Err(Failure::Panic(m)) if m == "boom 1"));
        let r = supervise(async { panic!("static") }).await;
        assert!(matches!(r, Err(Failure::Panic(m)) if m == "static"));
        let r = supervise(async { anyhow::bail!("plain") }).await;
        assert!(matches!(r, Err(Failure::Error(e)) if e.to_string() == "plain"));
        assert!(supervise(async { Ok(()) }).await.is_ok());
    }
}
