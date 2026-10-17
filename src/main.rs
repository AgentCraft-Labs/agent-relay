use agent_relay::{
    codex,
    model::*,
    provider, runner,
    runtime::{CliAdapter, RuntimeAdapter},
    store::Store,
};
use anyhow::{Context, Result, ensure};
use clap::{Args, Parser, Subcommand};
use serde_json::json;
use std::{io::Read, path::PathBuf, time::Duration};

#[derive(Parser)]
#[command(
    name = "agentrelay",
    version,
    about = "External CLI runtimes for native Codex agents"
)]
struct Cli {
    /// Private persistent registry directory (or AGENTRELAY_HOME).
    #[arg(long, global = true)]
    home: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Register immutable native model labels and their external execution settings.
    Profile {
        #[command(subcommand)]
        command: ProfileCommand,
    },
    /// Serve the local Responses provider; forward normal models to OpenAI.
    Serve {
        #[arg(long, default_value = "127.0.0.1:39091")]
        listen: std::net::SocketAddr,
    },
    /// Standalone diagnostic macOS bridge. Use codex migrate for managed activation.
    ServeOpenai {
        #[arg(long, default_value = "127.0.0.1:39092")]
        listen: std::net::SocketAddr,
        #[arg(long, default_value = "127.0.0.1:39091")]
        backend: std::net::SocketAddr,
        /// Exact trusted executable path; process names and HTTP headers are ignored.
        #[arg(long)]
        codex_bin: PathBuf,
    },
    /// Bind an explicit task to a native root and agent path before spawn/follow-up.
    Prepare {
        #[arg(long, default_value = "claude")]
        runtime: String,
        #[arg(long, default_value = ".")]
        workspace: PathBuf,
        #[arg(long)]
        prompt_file: Option<PathBuf>,
        #[arg(long)]
        model: Option<String>,
        /// Reasoning effort for the external CLI, separate from the native model alias.
        #[arg(long)]
        effort: Option<String>,
        #[arg(long, value_enum, default_value = "read-only")]
        access: Access,
        #[arg(long, default_value_t = 1800)]
        timeout_seconds: u64,
        #[arg(long)]
        root_id: Option<String>,
        #[arg(long)]
        agent_path: String,
    },
    /// Execute a new task in this foreground runner. Repeating a key never reruns it.
    Run(RunArgs),
    /// Resume an idle external session and consume queued messages in FIFO order.
    Resume {
        task: String,
        #[arg(long)]
        key: String,
        #[arg(long)]
        prompt_file: Option<PathBuf>,
    },
    /// Persist a queue-only message; this does not wake an idle runtime.
    Message {
        task: String,
        #[arg(long)]
        key: String,
        #[arg(long)]
        prompt_file: Option<PathBuf>,
    },
    /// Request cancellation of the active generation.
    Interrupt {
        task: String,
        #[arg(long, default_value = "User requested interruption")]
        reason: String,
    },
    /// Read one task, including its last external session ID.
    Show { task: String },
    /// List tasks, optionally restricted to one native Codex root session.
    List {
        #[arg(long)]
        root: Option<String>,
    },
    /// Read durable events after a cursor; optionally wait for new events.
    Events {
        task: String,
        #[arg(long, default_value_t = 0)]
        after: i64,
        #[arg(long,default_value_t=0,value_parser=clap::value_parser!(u64).range(0..=30))]
        wait_seconds: u64,
    },
    /// Reconcile expired runners, once or until interrupted. Never launches workers.
    Watchdog {
        #[arg(long)]
        once: bool,
    },
    /// Probe a runtime executable and report its actual supported contract.
    Doctor {
        #[arg(long, default_value = "claude")]
        runtime: String,
    },
    /// Install skills and roles, configure the provider, or export the integration.
    Codex {
        #[command(subcommand)]
        command: CodexCommand,
    },
}

#[derive(Args)]
struct RunArgs {
    #[arg(long, default_value = "claude")]
    runtime: String,
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    #[arg(long)]
    key: String,
    /// Read the task from a file; omit to read stdin. Prompts are never shell expressions.
    #[arg(long)]
    prompt_file: Option<PathBuf>,
    #[arg(long)]
    model: Option<String>,
    #[arg(long)]
    effort: Option<String>,
    #[arg(long, value_enum, default_value = "read-only")]
    access: Access,
    #[arg(long, default_value_t = 1800)]
    timeout_seconds: u64,
    #[arg(long)]
    root_id: Option<String>,
    #[arg(long)]
    agent_id: Option<String>,
    #[arg(long)]
    agent_path: Option<String>,
}

#[derive(Subcommand)]
enum ProfileCommand {
    Add {
        name: String,
        #[arg(long, default_value = "claude")]
        runtime: String,
        #[arg(long)]
        model: String,
        #[arg(long)]
        effort: Option<String>,
        #[arg(long)]
        display_name: Option<String>,
    },
    List,
}

#[derive(Subcommand)]
enum CodexCommand {
    /// Activate the built-in OpenAI bridge after both service listeners are ready.
    Migrate {
        #[arg(long)]
        codex_home: Option<PathBuf>,
        #[arg(long)]
        catalog: Option<PathBuf>,
        #[arg(long)]
        codex_bin: Option<PathBuf>,
        /// Additional native Codex executables; the native CLI on PATH is discovered too.
        #[arg(long)]
        also_codex_bin: Vec<PathBuf>,
        #[arg(long, default_value = "127.0.0.1:39091")]
        listen: std::net::SocketAddr,
        #[arg(long, default_value = "127.0.0.1:39092")]
        bridge_listen: std::net::SocketAddr,
    },
    /// Restore pre-bridge routing, preserving later preferences and chat history.
    Rollback {
        #[arg(long)]
        codex_home: Option<PathBuf>,
    },
    /// Upgrade the user integration and default profiles between external turns.
    Upgrade {
        #[arg(long)]
        wait_idle: bool,
        #[arg(long, default_value = "127.0.0.1:39091")]
        listen: std::net::SocketAddr,
    },
    /// Install/restart the per-user macOS LaunchAgent for the provider.
    ServiceInstall {
        #[arg(long, default_value = "127.0.0.1:39091")]
        listen: std::net::SocketAddr,
    },
    /// Restore a missing macOS provider registration without killing workers.
    ServiceEnsure {
        #[arg(long, default_value = "127.0.0.1:39091")]
        listen: std::net::SocketAddr,
    },
    /// Install a self-contained skill and native role, protecting user edits.
    Install {
        #[arg(long, required_unless_present = "user", conflicts_with = "user")]
        project: Option<PathBuf>,
        #[arg(long)]
        user: bool,
    },
    /// Configure a local provider and augmented model catalog; preserve a backup.
    Configure {
        #[arg(long)]
        codex_home: Option<PathBuf>,
        #[arg(long)]
        catalog: Option<PathBuf>,
        #[arg(long, default_value = "127.0.0.1:39091")]
        listen: std::net::SocketAddr,
    },
    /// Export a self-contained local plugin, including this executable.
    Export {
        #[arg(long)]
        out: PathBuf,
    },
}

fn read_prompt(path: Option<PathBuf>) -> Result<String> {
    let mut text = String::new();
    if let Some(path) = path {
        std::fs::File::open(&path)
            .with_context(|| format!("Cannot open {}", path.display()))?
            .take((MAX_FRAME + 1) as u64)
            .read_to_string(&mut text)?;
    } else {
        std::io::stdin()
            .take((MAX_FRAME + 1) as u64)
            .read_to_string(&mut text)?;
    }
    ensure!(
        !text.trim().is_empty() && text.len() <= MAX_FRAME,
        "Prompt must contain 1..={MAX_FRAME} bytes"
    );
    Ok(text)
}

fn default_home() -> Result<PathBuf> {
    if let Some(value) = std::env::var_os("AGENTRELAY_HOME") {
        return Ok(PathBuf::from(value));
    }
    let home = std::env::var_os("HOME").context("Pass --home or set AGENTRELAY_HOME")?;
    Ok(PathBuf::from(home).join(".local/state/agentrelay"))
}

#[tokio::main]
async fn main() {
    if let Err(error) = run(Cli::parse()).await {
        eprintln!("{}", json!({"error":format!("{error:#}")}));
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<()> {
    let home = cli.home.map(Ok).unwrap_or_else(default_home)?;
    match cli.command {
        Command::Codex {
            command:
                CodexCommand::Migrate {
                    codex_home,
                    catalog,
                    codex_bin,
                    also_codex_bin,
                    listen,
                    bridge_listen,
                },
        } => {
            let codex_home = codex_home.unwrap_or_else(default_codex_home);
            let primary = agent_relay::openai_bridge::executable(codex_bin.as_deref())?;
            let mut additional = agent_relay::transport::load(&home)?
                .map_or_else(Vec::new, |s| s.additional_codex_bins);
            for path in &also_codex_bin {
                additional.push(agent_relay::openai_bridge::executable(Some(path))?);
            }
            if let Some(path) = std::env::var_os("PATH").and_then(|value| {
                std::env::split_paths(&value)
                    .map(|dir| dir.join("codex"))
                    .find(|path| path.is_file())
            }) {
                match agent_relay::openai_bridge::executable(Some(&path)) {
                    Ok(path) => additional.push(path),
                    Err(error) if also_codex_bin.is_empty() => anyhow::bail!(
                        "Cannot resolve Codex CLI on PATH; pass --also-codex-bin with its actual native executable: {error:#}"
                    ),
                    Err(_) => {}
                }
            }
            additional.retain(|path| path != &primary);
            additional.sort();
            additional.dedup();
            let settings = agent_relay::transport::Openai {
                schema_version: 1,
                listen: bridge_listen,
                backend: listen,
                codex_bin: primary,
                additional_codex_bins: additional,
            };
            return agent_relay::migration::activate(
                &home,
                &codex_home,
                &catalog.unwrap_or_else(|| codex_home.join("models_cache.json")),
                &settings,
            );
        }
        Command::Codex {
            command: CodexCommand::Rollback { codex_home },
        } => {
            return agent_relay::migration::rollback(
                &home,
                &codex_home.unwrap_or_else(default_codex_home),
            );
        }
        Command::Codex {
            command: CodexCommand::Upgrade { wait_idle, listen },
        } => return codex::upgrade(&home, wait_idle, listen).await,
        Command::Codex {
            command: CodexCommand::ServiceInstall { listen },
        } => return codex::install_service(&home, listen),
        Command::Codex {
            command: CodexCommand::ServiceEnsure { listen },
        } => return agent_relay::service::ensure_service(&codex::user_root()?, &home, listen),
        Command::Codex {
            command: CodexCommand::Install { project, user },
        } => return codex::install(&project.unwrap_or(codex::user_root()?), user),
        Command::Codex {
            command:
                CodexCommand::Configure {
                    codex_home,
                    catalog,
                    listen,
                },
        } => {
            let codex_home = codex_home.unwrap_or_else(|| {
                std::env::var_os("CODEX_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| codex::user_root().expect("User home").join(".codex"))
            });
            return codex::configure(
                &home,
                &codex_home,
                &catalog.unwrap_or_else(|| codex_home.join("models_cache.json")),
                listen,
            );
        }
        Command::Codex {
            command: CodexCommand::Export { out },
        } => return codex::export(&out),
        _ => {}
    }
    let store = Store::open(&home)?;
    match cli.command {
        Command::Profile {
            command:
                ProfileCommand::Add {
                    name,
                    runtime,
                    model,
                    effort,
                    display_name,
                },
        } => {
            let profile = agent_relay::profiles::add(
                &home,
                &name,
                agent_relay::profiles::Profile {
                    runtime,
                    display_name: display_name.unwrap_or_else(|| model.clone()),
                    model,
                    effort,
                },
            )?;
            runner::emit(
                &json!({"profile":name,"selection":profile,"native_model":agent_relay::profiles::model_alias(&name),"native_role":agent_relay::profiles::role_name(&name),"configure_required":true,"new_conversation_required":true}),
            )?;
        }
        Command::Profile {
            command: ProfileCommand::List,
        } => runner::emit(&agent_relay::profiles::load(&home)?)?,
        Command::Serve { listen } => agent_relay::transport::serve(&home, listen).await?,
        Command::ServeOpenai {
            listen,
            backend,
            codex_bin,
        } => agent_relay::openai_bridge::serve(&home, listen, backend, &codex_bin).await?,
        Command::Prepare {
            runtime,
            workspace,
            prompt_file,
            model,
            effort,
            access,
            timeout_seconds,
            root_id,
            agent_path,
        } => {
            let root_id = root_id
                .or_else(|| std::env::var("CODEX_SESSION_ID").ok())
                .context("Pass --root-id or run inside a native Codex root")?;
            let mut request = RunRequest {
                runtime,
                workspace: std::fs::canonicalize(workspace)?,
                prompt: read_prompt(prompt_file)?,
                model,
                effort,
                access,
                root_id: None,
                agent_id: None,
                agent_path: None,
                timeout_secs: timeout_seconds,
            };
            let adapter = CliAdapter::resolve(&home, &request.runtime)?;
            adapter.normalize_request(&mut request)?;
            adapter.preflight(&request).await?;
            let selection = json!({"model":request.model,"effort":request.effort});
            let native = provider::Registry::open(&home)?.prepare(&provider::Assignment {
                root_id: root_id.clone(),
                agent_path: agent_path.clone(),
                request,
            })?;
            runner::emit(
                &json!({"prepared":true,"root_id":root_id,"agent_path":agent_path,"selection":selection,"native":native,"next_step":"spawn the returned native role/model, or follow up the existing matching native agent"}),
            )?;
        }
        Command::Run(args) => {
            let workspace =
                std::fs::canonicalize(args.workspace).context("Workspace must exist")?;
            ensure!(workspace.is_dir(), "Workspace must be a directory");
            let prompt = read_prompt(args.prompt_file)?;
            let adapter = CliAdapter::resolve(&home, &args.runtime)?;
            let root_id = args
                .root_id
                .or_else(|| std::env::var("CODEX_SESSION_ID").ok());
            let agent_id = args.agent_id.or_else(|| {
                std::env::var("CODEX_THREAD_ID")
                    .ok()
                    .filter(|id| Some(id) != root_id.as_ref())
            });
            let mut request = RunRequest {
                runtime: args.runtime,
                workspace,
                prompt,
                model: args.model,
                effort: args.effort,
                access: args.access,
                root_id,
                agent_id,
                agent_path: args.agent_path,
                timeout_secs: args.timeout_seconds,
            };
            adapter.normalize_request(&mut request)?;
            ensure!(
                request.agent_id.is_none() || request.agent_id != request.root_id,
                "Native agent ID must be distinct from the root ID"
            );
            let task = runner::execute(&store, store.begin(&args.key, request)?, adapter).await?;
            ensure!(
                task.status == Status::Completed || !task.status.terminal(),
                "Task {} ended as {:?}",
                task.id,
                task.status
            );
        }
        Command::Resume {
            task,
            key,
            prompt_file,
        } => {
            let previous = store.get(&task)?;
            let adapter = CliAdapter::resolve(&home, &previous.request.runtime)?;
            ensure!(
                adapter.capabilities().session_resume,
                "Runtime does not support resume"
            );
            let prompt = read_prompt(prompt_file)?;
            let task =
                runner::execute(&store, store.resume(&task, &key, &prompt)?, adapter).await?;
            ensure!(
                task.status == Status::Completed || !task.status.terminal(),
                "Task {} ended as {:?}",
                task.id,
                task.status
            );
        }
        Command::Message {
            task,
            key,
            prompt_file,
        } => runner::emit(&store.message(&task, &key, &read_prompt(prompt_file)?)?)?,
        Command::Interrupt { task, reason } => {
            runner::emit(&runner::summary(&store.interrupt(&task, &reason)?))?
        }
        Command::Show { task } => runner::emit(&store.get(&task)?)?,
        Command::List { root } => runner::emit(&store.list(root.as_deref())?)?,
        Command::Events {
            task,
            after,
            wait_seconds,
        } => {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(wait_seconds);
            loop {
                let events = store.events(&task, after)?;
                if !events.is_empty()
                    || store.get(&task)?.status.terminal()
                    || tokio::time::Instant::now() >= deadline
                {
                    let cursor = events.last().map(|event| event.sequence).unwrap_or(after);
                    runner::emit(
                        &json!({"task_id":task,"events":events,"cursor":cursor,"status":store.get(&task)?.status}),
                    )?;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        Command::Watchdog { once } => loop {
            let recovered = store.reconcile(now_ms())?;
            if once || recovered > 0 {
                runner::emit(&json!({"reconciled":recovered}))?;
            }
            if once {
                break;
            }
            tokio::select! { _=runner::shutdown_signal()=>break, _=tokio::time::sleep(Duration::from_secs(1))=>{} }
        },
        Command::Doctor { runtime } => {
            runner::emit(&CliAdapter::resolve(&home, &runtime)?.probe().await?)?
        }
        Command::Codex { .. } => unreachable!("handled before opening the registry"),
    }
    Ok(())
}

fn default_codex_home() -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| codex::user_root().expect("User home").join(".codex"))
}
