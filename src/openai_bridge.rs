//! Built-in OpenAI transport for Codex's built-in OpenAI provider on macOS.
//! The URL contains no credential. Admission uses kernel socket ownership;
//! the legacy provider still checks its installation-specific header key.
use anyhow::{Result, ensure};
use std::{net::SocketAddr, path::Path};

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use anyhow::Context;
    use axum::{
        Router,
        body::{Body, Bytes},
        extract::{ConnectInfo, DefaultBodyLimit, State},
        http::{HeaderMap, StatusCode},
        response::{IntoResponse, Response},
        routing::get,
    };
    use libproc::{
        bsd_info::BSDInfo,
        file_info::{ListFDs, pidfdinfo},
        net_info::SocketFDInfo,
        proc_pid::{listpidinfo, pidinfo, pidpath},
        processes::{ProcFilter, pids_by_type},
    };
    use std::{io::Read, net::Ipv4Addr, path::PathBuf, sync::Arc, time::Duration};

    /// Identify the owner of the exact reverse TCP tuple, not a process name
    /// or an HTTP-supplied PID. Recheck identity after inspecting descriptors.
    pub fn verify_peer(peer: SocketAddr, local: SocketAddr, executable: &Path) -> Result<u32> {
        ensure!(
            peer.ip().is_loopback() && local.ip().is_loopback(),
            "Nonlocal peer"
        );
        ensure!(peer.is_ipv4() && local.is_ipv4(), "IPv4 loopback required");
        // SAFETY: geteuid takes no arguments and has no preconditions.
        let uid = unsafe { libc::geteuid() };
        for pid in pids_by_type(ProcFilter::ByUID { uid })? {
            let pid = pid as i32;
            let Ok(path) = pidpath(pid) else { continue };
            if Path::new(&path) != executable {
                continue;
            }
            let Ok(before) = pidinfo::<BSDInfo>(pid, 0) else {
                continue;
            };
            if before.pbi_uid != uid || before.pbi_ruid != uid {
                continue;
            }
            let Ok(fds) = listpidinfo::<ListFDs>(pid, before.pbi_nfiles as usize) else {
                continue;
            };
            for fd in fds.into_iter().filter(|f| f.proc_fdtype == 2) {
                let Ok(socket) = pidfdinfo::<SocketFDInfo>(pid, fd.proc_fd) else {
                    continue;
                };
                if socket.psi.soi_kind != 2 || socket.psi.soi_family != libc::AF_INET {
                    continue;
                }
                // SAFETY: the kernel tagged this socket as TCP/AF_INET.
                let tcp = unsafe { socket.psi.soi_proto.pri_tcp };
                let ip = tcp.tcpsi_ini;
                if tcp.tcpsi_state != 4
                    || u16::from_be(ip.insi_lport as u16) != peer.port()
                    || u16::from_be(ip.insi_fport as u16) != local.port()
                {
                    continue;
                }
                // SAFETY: AF_INET selects the IPv4 union member.
                let (src, dst) = unsafe {
                    (
                        ip.insi_laddr.ina_46.i46a_addr4.s_addr,
                        ip.insi_faddr.ina_46.i46a_addr4.s_addr,
                    )
                };
                if Ipv4Addr::from(src.to_ne_bytes()) != peer.ip()
                    || Ipv4Addr::from(dst.to_ne_bytes()) != local.ip()
                {
                    continue;
                }
                let after = pidinfo::<BSDInfo>(pid, 0).map_err(anyhow::Error::msg)?;
                ensure!(
                    after.pbi_uid == uid
                        && after.pbi_ruid == uid
                        && before.pbi_start_tvsec == after.pbi_start_tvsec
                        && before.pbi_start_tvusec == after.pbi_start_tvusec
                        && pidpath(pid).ok().as_deref() == Some(path.as_str()),
                    "Peer identity changed"
                );
                return Ok(pid as u32);
            }
        }
        anyhow::bail!("Connection is not owned by the configured Codex executable")
    }

    struct Bridge {
        executables: Vec<PathBuf>,
        local: SocketAddr,
        backend: SocketAddr,
        key: String,
        client: reqwest::Client,
    }

    pub(crate) fn resolve_executable(entry: &Path) -> Result<PathBuf> {
        let entry = entry.canonicalize()?;
        let mut magic = [0; 4];
        std::fs::File::open(&entry)?.read_exact(&mut magic)?;
        if &magic[..2] != b"#!" {
            validate_native_executable(&entry, magic)?;
            return Ok(entry);
        }
        // The desktop distribution's bin/codex is a shell launcher. Resolve
        // its fixed bundle layout, never parse/evaluate arbitrary shell code.
        ensure!(
            entry.file_name().is_some_and(|name| name == "codex")
                && entry
                    .parent()
                    .and_then(Path::file_name)
                    .is_some_and(|name| name == "bin"),
            "Pass the actual Codex executable, not an arbitrary script"
        );
        let binary = entry
            .parent()
            .unwrap()
            .parent()
            .context("Missing Codex bundle")?
            .join("CodexCLI.app/Contents/MacOS/codex")
            .canonicalize()?;
        std::fs::File::open(&binary)?.read_exact(&mut magic)?;
        ensure!(
            &magic[..2] != b"#!",
            "Codex bundle target must be an executable"
        );
        validate_native_executable(&binary, magic)?;
        Ok(binary)
    }

    fn validate_native_executable(path: &Path, magic: [u8; 4]) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            [
                [0xcf, 0xfa, 0xed, 0xfe],
                [0xfe, 0xed, 0xfa, 0xcf],
                [0xca, 0xfe, 0xba, 0xbe],
                [0xbe, 0xba, 0xfe, 0xca],
                [0xca, 0xfe, 0xba, 0xbf],
                [0xbf, 0xba, 0xfe, 0xca]
            ]
            .contains(&magic),
            "Expected a native Mach-O Codex executable, not an interpreter or data file"
        );
        ensure!(
            std::fs::metadata(path)?.permissions().mode() & 0o111 != 0,
            "Codex file is not executable"
        );
        Ok(())
    }

    async fn admit(state: &Arc<Bridge>, peer: SocketAddr, headers: &HeaderMap) -> Result<()> {
        ensure!(
            !headers.contains_key("origin"),
            "Browser requests are not admitted"
        );
        let executables = state.executables.clone();
        let local = state.local;
        tokio::task::spawn_blocking(move || -> Result<()> {
            ensure!(
                executables
                    .iter()
                    .any(|path| verify_peer(peer, local, path).is_ok()),
                "Connection is not owned by a configured Codex executable"
            );
            Ok(())
        })
        .await??;
        Ok(())
    }

    async fn websocket(
        State(state): State<Arc<Bridge>>,
        ConnectInfo(peer): ConnectInfo<SocketAddr>,
        headers: HeaderMap,
    ) -> Response {
        if admit(&state, peer, &headers).await.is_err() {
            return StatusCode::FORBIDDEN.into_response();
        }
        // Codex explicitly interprets 426 as a session-scoped SSE fallback.
        StatusCode::UPGRADE_REQUIRED.into_response()
    }

    async fn responses(
        State(state): State<Arc<Bridge>>,
        ConnectInfo(peer): ConnectInfo<SocketAddr>,
        headers: HeaderMap,
        body: Bytes,
    ) -> Response {
        if admit(&state, peer, &headers).await.is_err() {
            return StatusCode::FORBIDDEN.into_response();
        }
        match forward(&state, headers, body).await {
            Ok(response) => response,
            // Never include credentials, request bodies or upstream URL errors.
            Err(_) => (StatusCode::BAD_GATEWAY, "Local provider request failed").into_response(),
        }
    }

    async fn forward(state: &Bridge, headers: HeaderMap, body: Bytes) -> Result<Response> {
        let encoding = headers
            .get("content-encoding")
            .map(|v| v.to_str())
            .transpose()?;
        let body = match encoding {
            None | Some("identity") => body.to_vec(),
            Some("zstd") => {
                let compressed = body.to_vec();
                tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
                    let mut data = Vec::new();
                    zstd::stream::read::Decoder::new(compressed.as_slice())?
                        .take((16 * crate::model::MAX_FRAME + 1) as u64)
                        .read_to_end(&mut data)?;
                    ensure!(
                        data.len() <= 16 * crate::model::MAX_FRAME,
                        "Decoded request too large"
                    );
                    Ok(data)
                })
                .await??
            }
            Some(_) => anyhow::bail!("Unsupported request encoding"),
        };
        let mut outgoing = state
            .client
            .post(format!("http://{}/responses", state.backend));
        for (name, value) in &headers {
            if !matches!(
                name.as_str(),
                "host"
                    | "content-length"
                    | "connection"
                    | "upgrade"
                    | "accept-encoding"
                    | "content-encoding"
                    | "transfer-encoding"
                    | crate::provider::KEY_HEADER
            ) {
                outgoing = outgoing.header(name, value);
            }
        }
        let response = outgoing
            .header(crate::provider::KEY_HEADER, &state.key)
            .body(body)
            .send()
            .await?;
        let mut result = Response::builder().status(response.status());
        for (name, value) in response.headers() {
            if !matches!(
                name.as_str(),
                "connection" | "transfer-encoding" | "content-length"
            ) {
                result = result.header(name, value);
            }
        }
        Ok(result.body(Body::from_stream(response.bytes_stream()))?)
    }

    pub async fn serve(
        home: &Path,
        address: SocketAddr,
        backend: SocketAddr,
        executable: &Path,
    ) -> Result<()> {
        let listener = tokio::net::TcpListener::bind(address).await?;
        let local = listener.local_addr()?;
        ensure!(local != backend, "Bridge must not forward to itself");
        let mut executables =
            vec![resolve_executable(executable).context("Cannot resolve Codex executable")?];
        if let Some(settings) = crate::transport::load(home)?
            && settings.listen == local
            && settings.backend == backend
        {
            for path in settings.additional_codex_bins {
                match resolve_executable(&path) {
                    Ok(path) => executables.push(path),
                    // A removed/upgraded optional CLI must not take the desktop
                    // transport down. Unresolved entries never authorize peers.
                    Err(_) => eprintln!("Ignoring unavailable additional Codex executable"),
                }
            }
        }
        executables.sort();
        executables.dedup();
        let state = Arc::new(Bridge {
            executables,
            local,
            backend,
            key: std::fs::read_to_string(home.join("provider.key"))?
                .trim()
                .to_owned(),
            client: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(5))
                .build()?,
        });
        ensure!(!state.key.is_empty(), "Empty local provider key");
        let version = state
            .client
            .get(format!("http://{backend}/version"))
            .timeout(Duration::from_secs(3))
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        let version: serde_json::Value = serde_json::from_slice(&version)?;
        ensure!(
            version["service"] == "AgentRelay" && version["native_checkpoints_v1"] == true,
            "Backend lacks native checkpoint support; update the provider before enabling the OpenAI bridge"
        );
        let router = Router::new()
            .route("/health", get(|| async { "AgentRelay OpenAI bridge" }))
            .route("/version",get(||async {axum::Json(serde_json::json!({"service":"AgentRelay","version":env!("CARGO_PKG_VERSION"),"transport":"openai_bridge","pid":std::process::id()}))}))
            .route("/responses", get(websocket).post(responses))
            .layer(DefaultBodyLimit::max(16 * crate::model::MAX_FRAME))
            .with_state(state);
        crate::runner::emit(
            &serde_json::json!({"openai_bridge_listening":local,"admission":"macos_socket_owner"}),
        )?;
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(crate::runner::shutdown_signal())
        .await?;
        Ok(())
    }
}

#[cfg(target_os = "macos")]
pub use macos::verify_peer;

#[cfg(target_os = "macos")]
pub(crate) use macos::resolve_executable;

pub fn executable(entry: Option<&Path>) -> Result<std::path::PathBuf> {
    #[cfg(target_os = "macos")]
    {
        if let Some(entry) = entry {
            return resolve_executable(entry);
        }
        for candidate in [
            "/Applications/ChatGPT.app/Contents/Resources/codex-cli/bin/codex",
            "/Applications/Codex.app/Contents/Resources/codex",
            "/Applications/Codex.app/Contents/Resources/codex-cli/bin/codex",
        ] {
            if let Ok(path) = resolve_executable(Path::new(candidate)) {
                return Ok(path);
            }
        }
        anyhow::bail!("Cannot find the official Codex executable; pass --codex-bin")
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = entry;
        anyhow::bail!("Built-in OpenAI bridge currently requires macOS")
    }
}

/// Start an isolated bridge. Existing user/provider configuration is untouched.
pub async fn serve(
    home: &Path,
    address: SocketAddr,
    backend: SocketAddr,
    executable: &Path,
) -> Result<()> {
    ensure!(
        address.ip().is_loopback()
            && backend.ip().is_loopback()
            && address.is_ipv4()
            && backend.is_ipv4(),
        "IPv4 loopback endpoints required"
    );
    #[cfg(target_os = "macos")]
    {
        macos::serve(home, address, backend, executable).await
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (home, executable);
        anyhow::bail!("Kernel peer admission is currently macOS-only")
    }
}
