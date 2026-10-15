use crate::{
    model::*,
    runtime::{CliAdapter, RuntimeAdapter},
    store::Store,
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{io::Write, process::Stdio, time::Duration};
use sysinfo::{Pid, ProcessesToUpdate, System};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::Child,
};

pub fn emit(value: &impl serde::Serialize) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer(&mut stdout, value)?;
    stdout.write_all(b"\n")?;
    stdout.flush()?;
    Ok(())
}

pub fn summary(task: &Task) -> Value {
    json!({"type":"task_result","task_id":task.id,"generation":task.generation,"runtime":task.request.runtime,"status":task.status,"session_id":task.session_id,"model":task.actual_model,"result":task.result,"error":task.error})
}

pub async fn execute(store: &Store, claim: Claim, mut adapter: CliAdapter) -> Result<Task> {
    emit(
        &json!({"type":"task","task_id":claim.task.id,"generation":claim.task.generation,"runtime":claim.task.request.runtime,"execute":claim.execute,"status":claim.task.status}),
    )?;
    if !claim.execute {
        emit(&summary(&claim.task))?;
        return Ok(claim.task);
    }
    let outcome = run_process(store, &claim, &mut adapter).await;
    let (status, result, error) = match outcome {
        Ok((status, result, error)) => (status, result, error),
        Err(error) => (Status::Failed, None, Some(format!("{error:#}"))),
    };
    let task = store.finish(&claim, status, result, error)?;
    emit(&summary(&task))?;
    Ok(task)
}

async fn run_process(
    store: &Store,
    claim: &Claim,
    adapter: &mut dyn RuntimeAdapter,
) -> Result<(Status, Option<String>, Option<String>)> {
    let launch = adapter.launch(&claim.task)?;
    let mut command = tokio::process::Command::new(&launch.binary);
    command
        .args(&launch.args)
        .current_dir(&claim.task.request.workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .spawn()
        .with_context(|| format!("Cannot launch runtime executable {}", launch.binary))?;
    let pid = child.id().context("Missing worker PID")?;
    let mut group = ProcessGroupGuard::new(pid);
    let identity = process_identity(pid);
    if !store.heartbeat(claim, identity)? {
        terminate(&mut child, &mut group).await;
        return Ok((
            Status::Interrupted,
            None,
            Some("Interrupted before runtime startup".into()),
        ));
    }
    let mut stdin = child.stdin.take().context("Missing runtime stdin")?;
    let input = tokio::spawn(async move {
        stdin.write_all(&launch.stdin).await?;
        stdin.shutdown().await
    });
    let stderr = child.stderr.take().context("Missing runtime stderr")?;
    let diagnostics = tokio::spawn(async move {
        let mut reader = stderr;
        let mut tail = Vec::<u8>::new();
        let mut buffer = [0; 4096];
        loop {
            let count = reader.read(&mut buffer).await?;
            if count == 0 {
                break;
            }
            tail.extend_from_slice(&buffer[..count]);
            if tail.len() > 8192 {
                tail.drain(..tail.len() - 8192);
            }
        }
        Ok::<_, std::io::Error>(String::from_utf8_lossy(&tail).into_owned())
    });
    let mut stdout = BufReader::new(child.stdout.take().context("Missing runtime stdout")?);
    let mut pending_frame = Vec::new();
    let mut heartbeat = tokio::time::interval(Duration::from_millis(250));
    let deadline =
        tokio::time::Instant::now() + Duration::from_secs(claim.task.request.timeout_secs);
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    let mut eof = false;
    let mut exit = None;
    let mut terminal: Option<std::result::Result<String, String>> = None;
    loop {
        if eof && exit.is_some() {
            break;
        }
        tokio::select! {
            _ = &mut shutdown => {
                input.abort();
                terminate(&mut child,&mut group).await;
                diagnostics.abort();
                return Ok((Status::Interrupted,None,Some("Runner received an interruption signal".into())));
            }
            _ = heartbeat.tick() => {
                if tokio::time::Instant::now() >= deadline {
                    input.abort();
                    terminate(&mut child,&mut group).await;
                    diagnostics.abort();
                    return Ok((Status::Failed,None,Some("Runtime exceeded its configured timeout".into())));
                }
                if !store.heartbeat(claim,None)? {
                    input.abort();
                    terminate(&mut child,&mut group).await;
                    diagnostics.abort();
                    return Ok((Status::Interrupted,None,Some("Task interrupted by its owner".into())));
                }
            }
            status = child.wait(), if exit.is_none() => {
                exit = Some(status?);
                // A runtime may leave descendants holding the output pipes open.
                group.kill();
                group.disarm();
            }
            line = read_frame(&mut stdout, &mut pending_frame), if !eof => {
                let Some(line) = line? else { eof=true; continue; };
                if line.trim().is_empty() { continue; }
                let frame: Value = serde_json::from_str(&line).context("Runtime emitted invalid JSON")?;
                for event in adapter.decode(frame)? {
                    match &event {
                        RuntimeEvent::Result { text,.. } => {
                            ensure!(terminal.is_none(),"Runtime emitted duplicate terminal results");
                            terminal=Some(Ok(text.clone()));
                        }
                        RuntimeEvent::Error(error) => {
                            ensure!(terminal.is_none(),"Runtime emitted duplicate terminal results");
                            terminal=Some(Err(error.clone()));
                        }
                        _ => {}
                    }
                    emit(&store.observe(claim,event)?)?;
                }
            }
        }
    }
    let input_result = input.await.context("Runtime stdin writer failed")?;
    let stderr = diagnostics
        .await
        .context("Runtime stderr reader failed")??;
    let status = exit.context("Missing runtime process exit")?;
    ensure!(
        status.success(),
        "Runtime exited with {status}: {}",
        stderr.trim()
    );
    input_result.context("Could not deliver the full runtime input")?;
    match terminal {
        Some(Ok(text)) => Ok((Status::Completed, Some(text), None)),
        Some(Err(error)) => Ok((Status::Failed, None, Some(error))),
        None => bail!(
            "Runtime exited without a successful terminal result: {}",
            stderr.trim()
        ),
    }
}

pub async fn read_frame<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    pending: &mut Vec<u8>,
) -> Result<Option<String>> {
    // read_until preserves pending bytes when select! cancels this future for a
    // heartbeat. read_line with a local String would silently lose partial frames.
    let remaining = MAX_FRAME.saturating_sub(pending.len()) + 1;
    let count = reader
        .take(remaining as u64)
        .read_until(b'\n', pending)
        .await?;
    ensure!(
        pending.len() <= MAX_FRAME,
        "Runtime frame exceeds {MAX_FRAME} bytes"
    );
    if count == 0 && pending.is_empty() {
        Ok(None)
    } else {
        Ok(Some(
            String::from_utf8(std::mem::take(pending)).context("Runtime frame is not UTF-8")?,
        ))
    }
}

struct ProcessGroupGuard {
    pid: u32,
    armed: bool,
}

impl ProcessGroupGuard {
    fn new(pid: u32) -> Self {
        Self { pid, armed: true }
    }
    fn kill(&self) {
        if self.armed {
            kill_group(self.pid, libc::SIGKILL);
        }
    }
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        self.kill();
    }
}

fn kill_group(pid: u32, signal: i32) {
    #[cfg(unix)]
    // The child was created as the leader of a fresh process group.
    unsafe {
        libc::kill(-(pid as i32), signal);
    }
    #[cfg(not(unix))]
    let _ = (pid, signal);
}

async fn terminate(child: &mut Child, group: &mut ProcessGroupGuard) {
    kill_group(group.pid, libc::SIGTERM);
    if tokio::time::timeout(Duration::from_millis(750), child.wait())
        .await
        .is_err()
    {
        group.kill();
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
    group.kill();
    group.disarm();
}

pub async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("SIGTERM handler");
        tokio::select! { _=tokio::signal::ctrl_c()=>{}, _=terminate.recv()=>{} }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

pub fn process_identity(pid: u32) -> Option<ProcessIdentity> {
    let mut system = System::new();
    let id = Pid::from_u32(pid);
    system.refresh_processes(ProcessesToUpdate::Some(&[id]), true);
    system.process(id).map(|process| ProcessIdentity {
        pid,
        start_time: process.start_time(),
    })
}

pub fn pid_exists(pid: u32) -> bool {
    process_identity(pid).is_some()
}

pub fn kill_verified_worker(identity: &ProcessIdentity) -> bool {
    let Some(current) = process_identity(identity.pid) else {
        return false;
    };
    if identity.start_time == 0 || current.start_time != identity.start_time {
        return false;
    }
    #[cfg(unix)]
    {
        // Do not signal a recycled PID or a process outside its recorded group.
        if unsafe { libc::getpgid(identity.pid as i32) } != identity.pid as i32 {
            return false;
        }
        kill_group(identity.pid, libc::SIGKILL);
        true
    }
    #[cfg(not(unix))]
    {
        false
    }
}
