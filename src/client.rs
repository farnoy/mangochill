use std::cell::RefCell;
use std::io::Write;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use anyhow::Context;
use anyhow::anyhow;
use anyhow::bail;
use clap::CommandFactory;
use clap::Parser;
use clap::error::ErrorKind;
use clap_verbosity_flag::Verbosity;
use log::{info, warn};
use mangochill::gamescope::GamescopeConnection;
use mangochill::mangochill_capnp::fps_receiver::{ReceiveParams, ReceiveResults};
use mangochill::{connect_rpc, mangochill_capnp, socket_path};
use regex::Regex;
use tokio::task::{LocalSet, spawn_local};
use tokio::time::{MissedTickBehavior, interval, timeout};
use tokio_util::sync::CancellationToken;

#[derive(Parser, Debug)]
#[command(name = "client", version, about = "MangoChill client")]
struct Cli {
    #[command(flatten)]
    verbosity: Verbosity,

    /// Max FPS that can ever be set
    #[arg(short = 't', long)]
    max_fps: Option<u16>,

    /// Min FPS that can ever be set
    #[arg(short = 'f', long, default_value_t = 30)]
    min_fps: u16,

    /// Attack (rising) half-life in milliseconds
    #[arg(long, default_value_t = 500.0)]
    attack_half_life_ms: f64,

    /// Release (falling) half-life in milliseconds
    #[arg(long, default_value_t = 5000.0)]
    release_half_life_ms: f64,

    /// FPS update frequency in Hz
    #[arg(long, default_value = "10")]
    frequency: u16,

    /// Unix socket path to the RPC server
    #[arg(short = 's', long)]
    rpc_socket: Option<PathBuf>,

    /// Snap FPS to the nearest integer divider of max-fps. Useful in non-VRR environments.
    #[arg(long)]
    snap: bool,

    /// Timeout in seconds waiting for backend socket to appear
    #[arg(long, default_value = "10")]
    socket_timeout_secs: u64,

    /// Command to spawn (e.g., -- mangohud game)
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<String>,
}

enum FpsSink {
    Gamescope(RefCell<GamescopeConnection>),
    MangoHud(RefCell<std::os::unix::net::UnixStream>),
}

impl FpsSink {
    fn detect_mangohud(child_pid: Option<u32>) -> Option<Self> {
        use std::os::linux::net::SocketAddrExt;
        let data = std::fs::read_to_string("/proc/net/unix").ok()?;
        let socket_name = if let Some(pid) = child_pid {
            let target = format!("mangohud-{pid}");
            data.contains(&format!("@{target}")).then_some(target)?
        } else {
            let regex = Regex::new(r"@(mangohud-\d+)").expect("regex invalid");
            regex.captures(&data)?.get(1)?.as_str().to_owned()
        };
        info!("using mangohud backend");
        info!("Using MangoHud socket: {socket_name}");
        let addr = std::os::unix::net::SocketAddr::from_abstract_name(&socket_name).ok()?;
        let stream = std::os::unix::net::UnixStream::connect_addr(&addr).ok()?;
        info!("MangoHud socket connected");
        Some(Self::MangoHud(RefCell::new(stream)))
    }
}

fn run_xprop(display: &str, props: &[&str]) -> anyhow::Result<String> {
    let mut cmd = std::process::Command::new("xprop");
    cmd.args(["-root", "-display", display]);
    cmd.args(props);
    let output = cmd
        .output()
        .with_context(|| format!("failed to run `xprop -root -display {display}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "`xprop -root -display {display}` exited with {}: {stderr}",
            output.status
        );
    }
    String::from_utf8(output.stdout).context("`xprop -root` returned non-UTF-8 output")
}

fn x11_display_numbers() -> Vec<u32> {
    let mut nums: Vec<u32> = std::fs::read_dir("/tmp/.X11-unix")
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            entry
                .ok()?
                .file_name()
                .to_str()?
                .strip_prefix('X')?
                .parse()
                .ok()
        })
        .collect();
    nums.sort();
    nums
}

fn detect_gamescope_max_fps(timeout: Duration) -> anyhow::Result<u16> {
    let deadline = std::time::Instant::now() + timeout;
    let focused_re =
        Regex::new(r"(?m)^GAMESCOPE_FOCUSED_WINDOW\(CARDINAL\) = \d+").expect("regex invalid");
    let refresh_re =
        Regex::new(r"(?m)^GAMESCOPE_DISPLAY_REFRESH_RATE_FEEDBACK\(CARDINAL\) = (\d+)")
            .expect("regex invalid");
    let displays: Vec<String> = x11_display_numbers()
        .into_iter()
        .map(|n| format!(":{n}"))
        .collect();

    info!("Waiting for gamescope to focus a window before reading refresh rate...");
    loop {
        for display in &displays {
            let stdout = run_xprop(
                display,
                &[
                    "GAMESCOPE_FOCUSED_WINDOW",
                    "GAMESCOPE_DISPLAY_REFRESH_RATE_FEEDBACK",
                ],
            );
            if let Ok(stdout) = stdout
                && focused_re.is_match(&stdout)
                && let Some(caps) = refresh_re.captures(&stdout)
                && let Ok(rate) = caps[1].parse()
            {
                info!("found gamescope properties on {display}");
                return Ok(rate);
            }
        }
        if std::time::Instant::now() >= deadline {
            bail!("timed out waiting for gamescope to focus a window");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

struct FpsReceiverImpl {
    sink: Rc<FpsSink>,
    min_fps: u16,
    max_fps: u16,
}

impl mangochill_capnp::fps_receiver::Server for FpsReceiverImpl {
    async fn receive(
        self: Rc<Self>,
        params: ReceiveParams,
        _: ReceiveResults,
    ) -> Result<(), capnp::Error> {
        let p = params.get()?;
        let fps = p.get_fps_limit();
        let target = (fps.round() as u16).clamp(self.min_fps, self.max_fps);
        match &*self.sink {
            FpsSink::Gamescope(conn) => {
                if let Err(e) = conn.borrow_mut().set_fps(target) {
                    return Err(capnp::Error::failed(format!("gamescope: {e}")));
                }
            }
            FpsSink::MangoHud(writer) => {
                let cmd = format!(":set_fps_limit={target};\n");
                if let Err(e) = writer.borrow_mut().write_all(cmd.as_bytes()) {
                    return Err(capnp::Error::failed(e.to_string()));
                }
            }
        }
        Ok(())
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    mangochill::init_logging(cli.verbosity.log_level_filter())?;

    // Spawn child first (if given) so backends have time to start.
    let mut child = if cli.command.is_empty() {
        None
    } else {
        let c = tokio::process::Command::new(&cli.command[0])
            .args(&cli.command[1..])
            .spawn()
            .map_err(|e| anyhow!("Failed to spawn `{}`: {e}", &cli.command[0]))?;
        let pid = c
            .id()
            .ok_or_else(|| anyhow!("Child exited immediately after spawn"))?;
        info!("Spawned child process (PID {pid}): {:?}", &cli.command);
        Some((c, pid))
    };

    let child_pid = child.as_ref().map(|(_, pid)| *pid);
    let mut poll = interval(Duration::from_millis(100));
    poll.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(cli.socket_timeout_secs);

    let sink = Rc::new(loop {
        if let Some(conn) = GamescopeConnection::try_connect() {
            info!("using gamescope backend");
            break FpsSink::Gamescope(RefCell::new(conn));
        }
        if let Some(sink) = FpsSink::detect_mangohud(child_pid) {
            break sink;
        }
        let Some((ref mut c, _)) = child else {
            return Err(anyhow!(
                "No backend found. Is gamescope running, or is MangoHud \
                configured with `control = mangohud-%p`?"
            ));
        };
        if tokio::time::Instant::now() >= deadline {
            return Err(anyhow!(
                "Timed out after {:?} waiting for a backend socket",
                Duration::from_secs(cli.socket_timeout_secs),
            ));
        }
        tokio::select! {
            _ = poll.tick() => {}
            status = c.wait() => {
                let status = status?;
                return Err(anyhow!(
                    "Child exited ({status}) before a backend socket appeared. \
                    Is gamescope running, or is MangoHud configured with \
                    `control = mangohud-%p`?"
                ));
            }
        }
    });

    let is_gamescope = matches!(*sink, FpsSink::Gamescope(_));
    let max_fps = match cli.max_fps {
        Some(max_fps) => max_fps,
        None if is_gamescope => {
            let max_fps = detect_gamescope_max_fps(Duration::from_secs(cli.socket_timeout_secs))
                .context(
                    "failed to auto-detect max FPS from gamescope.\
                    Specify --max-fps manually to skip auto-detection",
                )?;
            info!("Auto-detected max FPS from gamescope: {max_fps}");
            max_fps
        }
        None => Cli::command()
            .error(
                ErrorKind::MissingRequiredArgument,
                "--max-fps is required when not using gamescope",
            )
            .exit(),
    };

    let set = LocalSet::new();
    let _set_guard = set.enter();

    let rpc_socket = socket_path(cli.rpc_socket);

    let attack_half_life_us = (cli.attack_half_life_ms * 1000.0) as u32;
    let release_half_life_us = (cli.release_half_life_ms * 1000.0) as u32;
    let frequency = cli.frequency;
    let min_fps = cli.min_fps;
    let is_steam_deck = matches!(std::env::var("SteamDeck").as_deref(), Ok("1"));
    let snap = cli.snap || is_steam_deck;

    let ct = CancellationToken::new();
    let ct_rpc = ct.child_token();
    spawn_local(ct_rpc.clone().run_until_cancelled_owned(async move {
        let mut retry = interval(Duration::from_secs(1));
        retry.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            retry.tick().await;
            info!("Connecting to MangoChill server...");
            match connect_rpc(&rpc_socket).await {
                Ok((rpc_system, mango_chill)) => {
                    info!("Connected to MangoChill server");
                    let ct = ct_rpc.clone();
                    let rpc_bg = spawn_local(ct.run_until_cancelled_owned(rpc_system));

                    let fps_limiter = match timeout(
                        Duration::from_secs(5),
                        mango_chill.fps_limiter_request().send().promise,
                    )
                    .await
                    {
                        Ok(Ok(resp)) => resp.get().unwrap().get_service().unwrap(),
                        Ok(Err(e)) => {
                            warn!("Failed to get FpsLimiter service: {e}");
                            rpc_bg.abort();
                            continue;
                        }
                        Err(_) => {
                            warn!("Timed out getting FpsLimiter service");
                            rpc_bg.abort();
                            continue;
                        }
                    };

                    let receiver = FpsReceiverImpl {
                        sink: sink.clone(),
                        min_fps,
                        max_fps,
                    };
                    let receiver_client: mangochill_capnp::fps_receiver::Client =
                        capnp_rpc::new_client(receiver);
                    let mut req = fps_limiter.register_request();
                    req.get().set_frequency_hz(frequency);
                    req.get().set_min_fps(min_fps);
                    req.get().set_max_fps(max_fps);
                    req.get()
                        .set_attack_half_life_microseconds(attack_half_life_us);
                    req.get()
                        .set_release_half_life_microseconds(release_half_life_us);
                    req.get().set_receiver(receiver_client);
                    req.get().set_snap_to_divider(snap);

                    match timeout(Duration::from_secs(5), req.send().promise).await {
                        Ok(Ok(resp)) => {
                            let _sub = resp.get().unwrap().get_subscription().unwrap();
                            let _ = rpc_bg.await;
                            info!("RPC connection lost");
                        }
                        Ok(Err(e)) => {
                            warn!("Failed to register with FpsLimiter: {e}");
                            rpc_bg.abort();
                        }
                        Err(_) => {
                            warn!("Timed out registering with FpsLimiter");
                            rpc_bg.abort();
                        }
                    }
                }
                Err(e) => {
                    warn!("Failed to connect to MangoChill server: {e}");
                }
            }

            retry.tick().await;
        }
    }));

    set.run_until(async move {
        match child.as_mut() {
            None => {
                mangochill::termination_signal().await;
                info!("exiting");
            }
            Some((child, child_pid)) => {
                tokio::select! {
                    _ = mangochill::termination_signal() => {
                        info!("Signal received, shutting down child...");
                        unsafe { libc::kill(*child_pid as i32, libc::SIGTERM); }
                        if timeout(Duration::from_secs(5), child.wait()).await.is_err() {
                            warn!("Child did not exit after SIGTERM, sending SIGKILL");
                            let _ = child.kill().await;
                        }
                    }
                    _ = child.wait() => { }
                }
            }
        }
    })
    .await;

    ct.cancel();

    set.await;

    Ok(())
}
