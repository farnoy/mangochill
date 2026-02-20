use capnp::capability::Promise;
use capnp_rpc::{RpcSystem, pry};
use futures::FutureExt;
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    hint,
    io::{self, Read},
    os::{
        fd::{AsFd, AsRawFd},
        unix::fs::{OpenOptionsExt, PermissionsExt},
    },
    path::PathBuf,
    rc::Rc,
    task::{Poll, ready},
    time::Duration,
};
use tokio::{
    fs::remove_file,
    io::{AsyncRead, AsyncReadExt, unix::AsyncFd},
    join,
    net::UnixListener,
    sync::Notify,
    task::{LocalSet, spawn_local},
    time::MissedTickBehavior,
};
use tokio_stream::{StreamExt, wrappers::UnixListenerStream};
use tokio_util::{
    compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt},
    sync::CancellationToken,
    task::TaskTracker,
};

use clap::Parser;
use clap_verbosity_flag::Verbosity;
use log::{debug, info, warn};

use mangochill::{
    bootstrap::MangoChillImpl,
    fps_limiter::{FpsLimiterImpl, FpsSubscribers, feed_events},
    mangochill_capnp::{
        self,
        raw_events::{RegisterParams, RegisterResults},
    },
    monotonic_us, socket_path,
};

#[derive(Parser, Debug)]
#[command(name = "server", version, about = "Mangochill privileged server")]
struct Cli {
    #[command(flatten)]
    verbosity: Verbosity,

    /// Unix socket path to expose the RPC server on, defaults to /run/mangochill/server.sock
    #[arg(short = 's', long)]
    rpc_socket: Option<PathBuf>,

    /// Whether to expose raw event timestamps on the RPC socket
    #[arg(long, default_value = "false")]
    raw: bool,
}

struct RawSubscribers {
    map: HashMap<u32, RawSubscriber>,
}

struct RawServerImpl {
    next_id: u32,
    subscribers: Rc<RefCell<RawSubscribers>>,
    notify: Rc<Notify>,
}

struct RawSubscriber {
    id: u32,
    handle: mangochill_capnp::poll_receiver::Client,
}

impl mangochill_capnp::raw_events::Server for RawServerImpl {
    fn register(
        &mut self,
        params: RegisterParams,
        mut ret: RegisterResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let handle = pry!(p.get_receiver());
        self.subscribers.borrow_mut().map.insert(
            self.next_id,
            RawSubscriber {
                id: self.next_id,
                handle,
            },
        );
        self.notify.notify_one();
        let disconnector = capnp_rpc::new_client(RawDisconnector {
            id: self.next_id,
            subscribers: Rc::clone(&self.subscribers),
            notify: Rc::clone(&self.notify),
        });
        self.next_id += 1;
        ret.get().set_subscription(disconnector);
        Promise::ok(())
    }
}

struct RawDisconnector {
    id: u32,
    subscribers: Rc<RefCell<RawSubscribers>>,
    notify: Rc<Notify>,
}

impl mangochill_capnp::subscription::Server for RawDisconnector {}
impl Drop for RawDisconnector {
    fn drop(&mut self) {
        info!("disconnected raw subscriber {}", self.id);
        self.subscribers.borrow_mut().map.remove(&self.id);
        self.notify.notify_one();
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> io::Result<()> {
    let cli = Cli::parse();

    mangochill::init_logging(cli.verbosity.log_level_filter());

    let rpc_socket = socket_path(cli.rpc_socket);

    let fps_subscribers = Rc::new(RefCell::new(FpsSubscribers::new()));
    let raw_subscribers = Rc::new(RefCell::new(RawSubscribers {
        map: Default::default(),
    }));
    let notify = Rc::new(Notify::new());

    let ct = CancellationToken::new();
    let set = LocalSet::new();

    set.run_until(async {
        debug!("in run_until");
        let supervisor = spawn_local(evdev_supervisor(
            fps_subscribers.clone(),
            raw_subscribers.clone(),
            notify.clone(),
            ct.child_token(),
        ));
        let server = spawn_local(serve(
            cli.raw,
            rpc_socket,
            fps_subscribers.clone(),
            raw_subscribers.clone(),
            notify.clone(),
            ct.child_token(),
        ));
        debug!("waiting for signals");

        mangochill::termination_signal().await;

        info!("Signal received, terminating");

        ct.cancel();

        server.await??;
        supervisor.await??;

        io::Result::<()>::Ok(())
    })
    .await?;

    set.await;

    Ok(())
}

async fn serve(
    expose_raw: bool,
    rpc_socket: PathBuf,
    fps_subscribers: Rc<RefCell<FpsSubscribers>>,
    raw_subscribers: Rc<RefCell<RawSubscribers>>,
    notify: Rc<Notify>,
    ct: CancellationToken,
) -> io::Result<()> {
    let listener = UnixListener::bind(&rpc_socket).unwrap();
    std::fs::set_permissions(&rpc_socket, std::fs::Permissions::from_mode(0o666)).unwrap();
    let mut incoming = UnixListenerStream::new(listener);
    debug!("spawning rpc");

    let fps_limiter_client: mangochill_capnp::fps_limiter::Client = capnp_rpc::new_client(
        FpsLimiterImpl::new(fps_subscribers, ct.child_token(), Rc::clone(&notify)),
    );

    let raw_events_client: Option<mangochill_capnp::raw_events::Client> = if expose_raw {
        Some(capnp_rpc::new_client(RawServerImpl {
            next_id: 0,
            subscribers: raw_subscribers,
            notify,
        }))
    } else {
        None
    };

    let bootstrap: mangochill_capnp::mango_chill::Client =
        capnp_rpc::new_client(MangoChillImpl::new(fps_limiter_client, raw_events_client));

    debug!("spawned rpc");

    let child_ct = ct.child_token();

    match ct
        .run_until_cancelled_owned(async {
            while let Some(Ok(stream)) = incoming.next().await {
                let fd = stream.as_fd().as_raw_fd();
                info!("new stream {}, addr: {:?}", fd, stream.peer_addr());
                let (reader, writer) = stream.into_split();
                let reader = TokioAsyncReadCompatExt::compat(reader);
                let reader = futures::io::BufReader::new(reader);
                let writer = TokioAsyncWriteCompatExt::compat_write(writer);
                let writer = futures::io::BufWriter::new(writer);
                let network = capnp_rpc::twoparty::VatNetwork::new(
                    reader,
                    writer,
                    capnp_rpc::rpc_twoparty_capnp::Side::Server,
                    Default::default(),
                );

                let rpc_system = RpcSystem::new(Box::new(network), Some(bootstrap.clone().client));
                let disconnector = rpc_system.get_disconnector();
                let innerct = child_ct.child_token();
                let innerclone = innerct.clone();
                spawn_local(async move {
                    let on_cancel = async {
                        innerct.cancelled_owned().await;
                        info!("exiting stream {fd} due to cancellation and invoking disconnector");
                        disconnector.await.unwrap();
                        info!("disconnector done");
                    };
                    let rpc_run = async {
                        let res = rpc_system.await;
                        info!("dropped rpc system: {fd}, res: {res:?}");
                        innerclone.cancel();
                    };
                    join!(on_cancel, rpc_run);
                });
            }
        })
        .await
    {
        Some(_) => {
            info!("finished serve() cleanly");
        }
        None => {
            info!("cancelling serve");
        }
    }

    info!("deleting server socket");
    drop(incoming);
    remove_file(rpc_socket).await?;

    Ok(())
}

fn discover_mouse_devices() -> Vec<PathBuf> {
    let dir = match std::fs::read_dir("/dev/input/by-id") {
        Ok(d) => d,
        Err(e) => {
            warn!("cannot read /dev/input/by-id: {e}");
            return Vec::new();
        }
    };
    let mut paths = Vec::new();
    for entry in dir.flatten() {
        let name = entry.file_name();
        let is_event = name.to_str().unwrap().contains("-event");
        if is_event {
            match std::fs::canonicalize(entry.path()) {
                Ok(p) => paths.push(p),
                Err(e) => warn!("canonicalize {:?}: {e}", entry.path()),
            }
        }
    }
    paths
}

async fn evdev_supervisor(
    fps_subscribers: Rc<RefCell<FpsSubscribers>>,
    raw_subscribers: Rc<RefCell<RawSubscribers>>,
    notify: Rc<Notify>,
    ct: CancellationToken,
) -> io::Result<()> {
    let mut scan_interval = tokio::time::interval(Duration::from_secs(30));
    scan_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

    let tracked: Rc<RefCell<HashSet<PathBuf>>> = Rc::new(RefCell::new(HashSet::new()));
    let mut device_tracker: Option<(TaskTracker, CancellationToken)> = None;
    let mut next_device_id: u16 = 0;

    loop {
        tokio::select! {
            biased;
            _ = ct.cancelled() => break,
            _ = notify.notified() => {}
            _ = scan_interval.tick() => {}
        }

        let has_subscribers =
            !fps_subscribers.borrow().is_empty() || !raw_subscribers.borrow().map.is_empty();

        if has_subscribers {
            let discovered = discover_mouse_devices();
            let (tracker, device_ct) =
                device_tracker.get_or_insert_with(|| (TaskTracker::new(), ct.child_token()));
            for path in discovered {
                if !tracked.borrow().contains(&path) {
                    let device_id = next_device_id;
                    next_device_id = next_device_id.wrapping_add(1);
                    tracked.borrow_mut().insert(path.clone());
                    tracker.spawn_local(watch_device(
                        path,
                        device_id,
                        fps_subscribers.clone(),
                        raw_subscribers.clone(),
                        device_ct.child_token(),
                        Rc::clone(&tracked),
                    ));
                }
            }
        } else if let Some((tracker, device_ct)) = device_tracker.take() {
            device_ct.cancel();
            tracker.close();
            tracker.wait().await;
            tracked.borrow_mut().clear();
            info!("all devices closed (no subscribers)");
        }
    }

    if let Some((tracker, device_ct)) = device_tracker.take() {
        device_ct.cancel();
        tracker.close();
        tracker.wait().await;
    }

    info!("evdev_supervisor exiting");
    Ok(())
}

async fn watch_device(
    path: PathBuf,
    device_id: u16,
    fps_subscribers: Rc<RefCell<FpsSubscribers>>,
    raw_subscribers: Rc<RefCell<RawSubscribers>>,
    ct: CancellationToken,
    tracked: Rc<RefCell<HashSet<PathBuf>>>,
) {
    info!("watching {:?} as device {device_id}", path.to_str());
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&path)
    {
        Ok(f) => f,
        Err(e) => {
            warn!("failed to open {:?}: {e}", path);
            tracked.borrow_mut().remove(&path);
            return;
        }
    };
    let fd = file.as_raw_fd();

    nix::ioctl_write_ptr!(eviocsclockid, b'E', 0xa0, libc::c_int);
    if let Err(e) = unsafe { eviocsclockid(fd, &libc::CLOCK_MONOTONIC) } {
        warn!("EVIOCSCLOCKID failed on {path:?}: {e}");
    }

    let mut file = AsyncCharDev::new(file).unwrap();
    let mut buf = [0u8; size_of::<libc::input_event>() * 64];
    let mut timestamps_us: Vec<i64> = Vec::new();
    let cancellation = ct.cancelled_owned().fuse();
    tokio::pin!(cancellation);

    loop {
        let read;
        timestamps_us.clear();
        tokio::select! {
            biased;
            _ = &mut cancellation => {
                info!("cancelled device {fd}");
                break;
            }
            res = file.read(&mut buf).fuse() => {
                match res {
                    Ok(n) => {
                        read = n;
                        debug!("read {read}");
                    }
                    Err(e) => {
                        warn!("device {device_id} (fd {fd}) read error: {e}");
                        break;
                    }
                }
            }
        };
        let aligned = unsafe {
            let (l, m, r) = buf[..read].align_to::<libc::input_event>();
            assert!(
                l.is_empty() && r.is_empty(),
                "could not align read input event to the C struct"
            );
            debug!("have: {} events", m.len());
            m
        };

        timestamps_us.reserve(aligned.len());
        let mut prev = i64::MIN;
        for ev in aligned {
            let ts_us = ev.time.tv_sec * 1_000_000 + ev.time.tv_usec;
            if (ts_us != prev) {
                timestamps_us.push(ts_us);
            }
            prev = ts_us;
        }

        feed_events(&mut fps_subscribers.borrow_mut(), device_id, &timestamps_us);

        if !raw_subscribers.borrow().map.is_empty() {
            let now_us = monotonic_us();
            for sub in raw_subscribers.borrow().map.values() {
                let id = sub.id;
                let mut req = sub.handle.receive_request();
                let mut ts = req.get().init_collected_at();
                ts.set_seconds(now_us / 1_000_000);
                ts.set_microseconds((now_us % 1_000_000) as u32);
                let count = u32::try_from(aligned.len()).unwrap();
                let mut readings = req.get().init_readings(count);
                for (ix, ev) in aligned.iter().enumerate() {
                    let mut r = readings.reborrow().get(u32::try_from(ix).unwrap());
                    r.set_seconds(ev.time.tv_sec);
                    r.set_microseconds(ev.time.tv_usec as u32);
                }
                spawn_local(async move {
                    if let Err(e) = req.send().promise.await {
                        warn!("Error sending to raw subscriber {id} - {e:?}");
                    }
                });
            }
        }
    }

    tracked.borrow_mut().remove(&path);
    info!("device {device_id} ({:?}) stopped", path);
}

struct AsyncCharDev {
    inner: AsyncFd<std::fs::File>,
}

impl AsyncCharDev {
    fn new(file: std::fs::File) -> io::Result<Self> {
        Ok(AsyncCharDev {
            inner: AsyncFd::new(file)?,
        })
    }
}

impl AsyncRead for AsyncCharDev {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        loop {
            let mut guard = ready!(self.inner.poll_read_ready(cx))?;
            let unfilled = buf.initialize_unfilled();
            match guard.try_io(|inner| inner.get_ref().read(unfilled)) {
                Ok(Ok(len)) => {
                    buf.advance(len);
                    return Poll::Ready(Ok(()));
                }
                Ok(Err(err)) => return Poll::Ready(Err(err)),
                Err(_would_block) => continue,
            }
        }
    }
}
