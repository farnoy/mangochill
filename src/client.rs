use std::cell::RefCell;
use std::io::Write;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use anyhow::anyhow;
use capnp::capability::Promise;
use capnp_rpc::pry;
use clap::Parser;
use clap_verbosity_flag::Verbosity;
use log::{info, warn};
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
    max_fps: u16,

    /// Min FPS that can ever be set
    #[arg(short = 'f', long)]
    min_fps: u16,

    /// Attack (rising) half-life in milliseconds
    #[arg(long, default_value_t = 500.0)]
    attack_half_life_ms: f64,

    /// Release (falling) half-life in milliseconds
    #[arg(long, default_value_t = 5000.0)]
    release_half_life_ms: f64,

    /// FPS update frequency in Hz
    #[arg(long, default_value = "200")]
    frequency: u16,

    /// Unix socket path to the RPC server
    #[arg(short = 's', long)]
    rpc_socket: Option<PathBuf>,
}

struct FpsReceiverImpl {
    writer: Rc<RefCell<std::os::unix::net::UnixStream>>,
    min_fps: u16,
    max_fps: u16,
}

impl mangochill_capnp::fps_receiver::Server for FpsReceiverImpl {
    fn receive(&mut self, params: ReceiveParams, _: ReceiveResults) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let fps = p.get_fps_limit();
        let target = (fps.round() as u16).clamp(self.min_fps, self.max_fps);
        let cmd = format!(":set_fps_limit={target};\n");
        if let Err(e) = self.writer.borrow_mut().write_all(cmd.as_bytes()) {
            return Promise::err(capnp::Error::failed(e.to_string()));
        }
        Promise::ok(())
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    mangochill::init_logging(cli.verbosity.log_level_filter());

    let ct = CancellationToken::new();

    let data = std::fs::read_to_string("/proc/net/unix")?;
    let regex = Regex::new(r"@(mangohud-\d+)").expect("regex invalid");
    let Some(m) = regex.captures(&data) else {
        return Err(anyhow!(
            "MangoHud socket not found, is it running? Have you configured `control = mangohud-%p`?"
        ));
    };
    let socket_name = m.get(1).unwrap().as_str();
    info!("Using MangoHud socket: {socket_name}");
    let addr =
        <std::os::unix::net::SocketAddr as std::os::linux::net::SocketAddrExt>::from_abstract_name(
            socket_name,
        )?;
    let stream = std::os::unix::net::UnixStream::connect_addr(&addr)?;
    let writer = Rc::new(RefCell::new(stream));
    info!("MangoHud socket connected");

    let set = LocalSet::new();
    let _set_guard = set.enter();

    let rpc_socket = socket_path(cli.rpc_socket);

    let attack_half_life_us = (cli.attack_half_life_ms * 1000.0) as u32;
    let release_half_life_us = (cli.release_half_life_ms * 1000.0) as u32;
    let frequency = cli.frequency;
    let min_fps = cli.min_fps;
    let max_fps = cli.max_fps;

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
                        writer: writer.clone(),
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

    set.run_until(async {
        let _guard = ct.drop_guard_ref();
        mangochill::termination_signal().await;
        info!("exiting");
    })
    .await;

    ct.cancel();

    set.await;

    Ok(())
}
