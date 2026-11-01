//! Shared subprocess runner: piped stdio, one time budget, kill and reap on expiry.

use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

pub struct Piped {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[derive(Debug)]
pub enum RunError {
    Spawn(std::io::Error),
    Io(std::io::Error),
    Timeout,
}

/// Runs `program args` with `input` on stdin. Spawning happens first; one `budget` then covers
/// stdin, draining stdout and stderr concurrently, and `wait`; on expiry the child is killed
/// and reaped.
pub async fn run_piped(
    program: &str,
    args: &[&str],
    input: &[u8],
    budget: Duration,
) -> Result<Piped, RunError> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(RunError::Spawn)?;
    let mut stdin = child.stdin.take();
    let mut stdout = child.stdout.take().expect("stdout piped");
    let mut stderr = child.stderr.take().expect("stderr piped");

    // `child` stays outside the timed future so it can be killed on expiry.
    let run = async {
        if let Some(mut s) = stdin.take() {
            // A child that exits without reading stdin gives EPIPE; its exit status decides.
            let _ = s.write_all(input).await;
        }
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let (o, e, st) = tokio::join!(
            stdout.read_to_end(&mut out),
            stderr.read_to_end(&mut err),
            child.wait()
        );
        o.and(e).map_err(RunError::Io)?;
        Ok::<_, RunError>(Piped {
            status: st.map_err(RunError::Io)?,
            stdout: out,
            stderr: err,
        })
    };
    match tokio::time::timeout(budget, run).await {
        Ok(r) => r,
        Err(_) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            Err(RunError::Timeout)
        }
    }
}
