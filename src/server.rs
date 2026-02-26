use capnp_rpc::RpcSystem;
use futures::future::{Either, select};
use nix::request_code_read;
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    io::{self, Read},
    mem::size_of,
    os::{
        fd::{AsFd, AsRawFd},
        unix::fs::{OpenOptionsExt, PermissionsExt},
    },
    path::PathBuf,
    pin::pin,
    rc::Rc,
    time::Duration,
};
use tokio::{
    fs::remove_file,
    io::{Interest, unix::AsyncFd},
    join,
    net::UnixListener,
    sync::Notify,
    task::{LocalSet, spawn_local},
    time::{Interval, MissedTickBehavior, interval},
};
use tokio_stream::{StreamExt, wrappers::UnixListenerStream};
use tokio_util::{
    compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt},
    sync::CancellationToken,
    task::TaskTracker,
};
use tracing::trace_span;

use clap::Parser;
use clap_verbosity_flag::Verbosity;
use log::{debug, error, info, trace, warn};

use mangochill::{
    bootstrap::MangoChillImpl,
    fps_limiter::{FpsLimiterImpl, FpsSubscribers, feed_events},
    input_parsing::{self, AXIS_COUNT, Axes, Buttons, DeviceCapabilities, EV_ABS, EV_KEY},
    mangochill_capnp::{
        self,
        raw_events::{RegisterParams, RegisterResults},
    },
    monotonic_us,
    scratch::Scratch,
    socket_path,
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
    next_id: RefCell<u32>,
    subscribers: Rc<RefCell<RawSubscribers>>,
    notify: Rc<Notify>,
}

struct RawSubscriber {
    id: u32,
    handle: mangochill_capnp::poll_receiver::Client,
}

impl mangochill_capnp::raw_events::Server for RawServerImpl {
    async fn register(
        self: Rc<Self>,
        params: RegisterParams,
        mut ret: RegisterResults,
    ) -> Result<(), capnp::Error> {
        let p = params.get()?;
        let handle = p.get_receiver()?;
        let id = {
            let mut cell = self.next_id.borrow_mut();
            let i = *cell;
            *cell += 1;
            i
        };
        self.subscribers
            .borrow_mut()
            .map
            .insert(id, RawSubscriber { id, handle });
        self.notify.notify_one();
        let disconnector = capnp_rpc::new_client(RawDisconnector {
            id,
            subscribers: Rc::clone(&self.subscribers),
            notify: Rc::clone(&self.notify),
        });
        ret.get().set_subscription(disconnector);
        Ok(())
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

#[cfg(feature = "tracy")]
register_demangler!();

#[tokio::main(flavor = "current_thread")]
async fn main() -> io::Result<()> {
    let cli = Cli::parse();

    #[allow(unused_labels)]
    'init: {
        #[cfg(feature = "tracy")]
        {
            tracing_log::LogTracer::init_with_filter(cli.verbosity.log_level_filter())
                .expect("failed to set up LogTracer");
            tracing::subscriber::set_global_default(
                tracing_subscriber::registry().with(tracing_tracy::TracyLayer::default()),
            )
            .expect("setup tracy layer");
            break 'init;
        }

        #[allow(unreachable_code)]
        {
            mangochill::init_logging(cli.verbosity.log_level_filter());
        }
    }

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
            next_id: RefCell::new(0),
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
    let dir = match std::fs::read_dir("/dev/input") {
        Ok(d) => d,
        Err(e) => {
            warn!("cannot read /dev/input: {e}");
            return Vec::new();
        }
    };
    let mut paths = Vec::new();
    for entry in dir.flatten() {
        let name = entry.file_name();
        if name.as_encoded_bytes().starts_with(b"event") {
            paths.push(entry.path());
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
                        Scratch::new(),
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
    scratch: Rc<RefCell<Scratch>>,
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
        error!("EVIOCSCLOCKID failed on {path:?}: {e}; aborting device");
        return;
    }

    nix::ioctl_read_buf!(eviocgprop, b'E', 0x09, libc::c_uchar);
    let input_prop_pointer =
        u8::try_from(libc::INPUT_PROP_POINTER).expect("INPUT_PROP_POINTER must fit into u8");
    let input_prop_accelerometer = u8::try_from(libc::INPUT_PROP_ACCELEROMETER)
        .expect("INPUT_PROP_ACCELEROMETER must fit into u8");
    let mut properties = [0u8; 1];
    if let Err(e) = unsafe { eviocgprop(fd, &mut properties) } {
        warn!("EVIOCGPROP failed on {path:?}: {e}");
        return;
    }
    let is_pointer = properties[0] & (1u8 << input_prop_pointer) != 0;
    let is_accelerometer = properties[0] & (1u8 << input_prop_accelerometer) != 0;
    let (has_abs, has_key) = {
        let ev_max = usize::from(libc::EV_MAX);
        let event_bits_len = (ev_max + 8) / 8;
        let mut event_bits = vec![0u8; event_bits_len];
        let req = request_code_read!(b'E', 0x20, event_bits_len);
        match unsafe { nix::errno::Errno::result(libc::ioctl(fd, req, event_bits.as_mut_ptr())) } {
            Ok(_) => {
                let has_abs = event_bits
                    .get(usize::from(EV_ABS) / 8)
                    .is_some_and(|byte| byte & (1 << (usize::from(EV_ABS) % 8)) != 0);
                let has_key = event_bits
                    .get(usize::from(EV_KEY) / 8)
                    .is_some_and(|byte| byte & (1 << (usize::from(EV_KEY) % 8)) != 0);
                (has_abs, has_key)
            }
            Err(e) => {
                error!("EVIOCGBIT failed on {path:?}: {e}; aborting device");
                return;
            }
        }
    };
    let caps = DeviceCapabilities {
        is_pointer,
        is_accelerometer,
        has_abs,
        has_key,
    };
    if caps.is_accelerometer {
        info!("ignoring accelerometer device {path:?}");
        // TODO: should keep it open so we know when it disappears & them remove.
        //       currently, if sth else was plugged into this eventN slot,
        //       we would keep ignoring it?
        return;
    }

    let file = AsyncFd::with_interest(file, Interest::READABLE).unwrap();
    let res = if caps.has_abs {
        #[repr(C)]
        #[allow(non_camel_case_types)]
        #[derive(Default, Clone, Copy, Debug)]
        struct input_absinfo {
            value: i32,
            minimum: i32,
            maximum: i32,
            fuzz: i32,
            flat: i32,
            resolution: i32,
        }
        let mut absinfo = [input_absinfo::default(); AXIS_COUNT];
        let mut gabs = |i: usize| unsafe {
            nix::errno::Errno::result(libc::ioctl(
                fd,
                request_code_read!(b'E', 0x40 + i, size_of::<input_absinfo>()),
                &mut absinfo[i..],
            ))
        };
        for i in 0..AXIS_COUNT {
            if let Err(e) = gabs(i) {
                warn!("EVIOCGABS failed on {path:?}: {e}");
                break;
            }
        }
        let mut min_max = [(0i32, 0i32); AXIS_COUNT];
        for i in 0..AXIS_COUNT {
            min_max[i] = (absinfo[i].minimum, absinfo[i].maximum);
        }
        let axes = Axes::from_min_max(&min_max);

        if caps.has_key {
            ct.run_until_cancelled_owned(watch_device_loop::<true, true>(
                path.clone(),
                device_id,
                fd,
                file,
                fps_subscribers,
                raw_subscribers,
                scratch,
                (axes, Buttons::default(), caps.is_pointer),
            ))
            .await
        } else {
            ct.run_until_cancelled_owned(watch_device_loop::<true, false>(
                path.clone(),
                device_id,
                fd,
                file,
                fps_subscribers,
                raw_subscribers,
                scratch,
                (axes, caps.is_pointer),
            ))
            .await
        }
    } else if caps.has_key {
        ct.run_until_cancelled_owned(watch_device_loop::<false, true>(
            path.clone(),
            device_id,
            fd,
            file,
            fps_subscribers,
            raw_subscribers,
            scratch,
            Buttons::default(),
        ))
        .await
    } else {
        ct.run_until_cancelled_owned(watch_device_loop::<false, false>(
            path.clone(),
            device_id,
            fd,
            file,
            fps_subscribers,
            raw_subscribers,
            scratch,
            (),
        ))
        .await
    };

    match res {
        Some(Ok(())) => {}
        Some(Err(e)) => warn!("Error in watch_device: {e}"),
        None => info!("cancelled device {fd}"),
    }

    tracked.borrow_mut().remove(&path);
    info!("device {device_id} ({:?}) stopped", path);
}

#[allow(clippy::too_many_arguments)]
async fn watch_device_loop<const HAS_ABS: bool, const HAS_KEY: bool>(
    path: PathBuf,
    device_id: u16,
    fd: i32,
    mut file: AsyncFd<std::fs::File>,
    fps_subscribers: Rc<RefCell<FpsSubscribers>>,
    raw_subscribers: Rc<RefCell<RawSubscribers>>,
    scratch: Rc<RefCell<Scratch>>,
    mut state: <input_parsing::Processor as input_parsing::ProcessorState<HAS_ABS, HAS_KEY>>::State,
) -> io::Result<()>
where
    input_parsing::Processor: input_parsing::ProcessorState<HAS_ABS, HAS_KEY>,
{
    // The debouncer delays our reads from the chardev after it first becomes readable.
    // It also interacts with the late polling feature
    let mut debouncer = Debounce::new(Duration::from_millis(1));
    let mut ready_guard = file.ready_mut(Interest::READABLE).await?;
    let mut is_ready = false;
    let mut was_interrupted = false;
    loop {
        let prolog_was_ready = is_ready;
        let prolog_was_interrupted = was_interrupted;

        if is_ready {
            trace!("repeating read immediately");
        } else {
            ready_guard = file.ready_mut(Interest::READABLE).await?;
            debouncer.restart();
            is_ready = true;
            was_interrupted = false;
            let mut rx = fps_subscribers.borrow().interrupt_receiver();
            let res = debouncer.tick(rx.recv()).await;
            match res {
                DebounceResult::SleepCompleted => trace!("successfully slept"),
                DebounceResult::Interrupted(Ok(())) => {
                    was_interrupted = true;
                    trace!("got interrupted")
                }
                DebounceResult::Interrupted(Err(e)) => {
                    error!("error waiting for interrupt receiver: {e}")
                }
            }
        }
        debug_assert!(
            ready_guard.ready().is_readable(),
            "I don't understand tokio"
        );
        let s = trace_span!(
            "watch_device::read_io",
            device_id,
            prolog_was_ready,
            is_ready,
            prolog_was_interrupted,
            was_interrupted,
        );
        let _g = s.enter();

        let x = ready_guard.try_io(|f| {
            // NOTE: we will reborrow after, important not to suspend until then
            trace!("read in async_io");
            let mut scratch = scratch.borrow_mut();
            unsafe { f.get_mut().read(scratch.read_buffer_mut()) }
        });

        let read = match x {
            Ok(Ok(n)) => n,
            Ok(Err(e)) => {
                warn!("device {device_id} (fd {fd}) read error: {e}");
                break;
            }
            Err(_would_block) => {
                is_ready = false;
                continue;
            }
        };

        let mut scratch = scratch.borrow_mut();
        let read_full = read == scratch.read_buffer_len();
        debug!(
            "read {read}, full={read_full} because scratch len={}",
            scratch.read_buffer_len()
        );
        let outcome = {
            let span = trace_span!("watch_device::process_loop", device_id);
            let _g = span.enter();
            unsafe {
                <input_parsing::Processor as input_parsing::ProcessorState<
                    HAS_ABS,
                    HAS_KEY,
                >>::process_batch(&mut scratch, read, &mut state)
            }
        };

        if outcome.overflowed {
            warn!("OVERFLOWED");
            debouncer.shrink();
        } else if !read_full && !was_interrupted {
            debouncer.expand();
        }
        let s = trace_span!(
            "watch_device::feed_events",
            path = path.to_str().unwrap(),
            device_id,
            read_bytes = read,
            filled_scratch = read_full,
            overflowed = outcome.overflowed,
            compacted = outcome.timestamps_us.len()
        );
        let _g = s.enter();

        feed_events(
            &mut fps_subscribers.borrow_mut(),
            device_id,
            outcome.timestamps_us,
            outcome.is_active_indefinitely,
        );

        if !raw_subscribers.borrow().map.is_empty() {
            let now_us = monotonic_us();
            for sub in raw_subscribers.borrow().map.values() {
                let id = sub.id;
                let mut req = sub.handle.receive_request();
                let mut ts = req.get().init_collected_at();
                ts.set_seconds(now_us / 1_000_000);
                ts.set_microseconds((now_us % 1_000_000) as u32);
                let count = u32::try_from(outcome.timestamps_us.len()).unwrap();
                let mut readings = req.get().init_readings(count);
                for (ix, ts) in outcome.timestamps_us.iter().enumerate() {
                    let mut r = readings.reborrow().get(u32::try_from(ix).unwrap());
                    r.set_seconds(ts / 1_000_000);
                    r.set_microseconds((ts % 1_000_000) as u32);
                }
                spawn_local(async move {
                    if let Err(e) = req.send().promise.await {
                        warn!("Error sending to raw subscriber {id} - {e:?}");
                    }
                });
            }
        }
    }

    Ok(())
}

// trailing-edge throttle
struct Debounce {
    interval: Interval,
    ceiling: Duration,
}

enum DebounceResult<T> {
    SleepCompleted,
    Interrupted(T),
}

impl Debounce {
    fn new(d: Duration) -> Self {
        let mut interval = interval(d);
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        interval.reset();
        Debounce {
            interval,
            ceiling: Duration::MAX,
        }
    }

    async fn tick<F, R>(&mut self, fut: F) -> DebounceResult<R>
    where
        F: Future<Output = R>,
    {
        match select(pin!(self.interval.tick()), pin!(fut)).await {
            Either::Left((_value, _)) => DebounceResult::SleepCompleted,
            Either::Right((v, _)) => DebounceResult::Interrupted(v),
        }
    }

    fn expand(&mut self) {
        let period = self.interval.period();
        let target = (period * 2).min(self.ceiling);
        self.reset(target);
    }

    fn shrink(&mut self) {
        let period = self.interval.period();
        let target = period * 4 / 5;
        self.ceiling = self.ceiling.min(target);
        self.reset(target);
    }

    fn restart(&mut self) {
        self.interval.reset_after(self.interval.period());
    }

    fn reset(&mut self, duration: Duration) {
        if self.interval.period() == duration {
            return;
        }
        self.interval = interval(duration);
        info!("scaling feedback window to {:?}", duration);
        self.interval
            .set_missed_tick_behavior(MissedTickBehavior::Skip);
        self.interval.reset();
    }
}

#[cfg(test)]
mod test {
    #[tokio::test(flavor = "current_thread")]
    #[cfg(not(miri))]
    // sanity check
    async fn test_debounce() {
        use std::{
            cell::{Cell, RefCell},
            rc::Rc,
            time::Duration,
        };
        use tokio::{
            task::{LocalSet, spawn_local},
            time::Instant,
            time::sleep,
        };

        let timer = Rc::new(RefCell::new(Box::pin(sleep(Duration::from_secs(10)))));

        let set = LocalSet::new();
        let _guard = set.enter();

        let completed = Rc::new(Cell::new(false));

        spawn_local({
            let timer = timer.clone();
            let completed = completed.clone();
            async move {
                dbg!("in s start");
                timer.borrow_mut().as_mut().await;
                dbg!("in s setting completed");
                completed.set(true);
            }
        });
        assert!(!completed.get());

        set.run_until(async move {
            assert!(!completed.get());
            timer.borrow_mut().as_mut().reset(Instant::now());
            assert!(!completed.get());
            sleep(Duration::from_millis(1)).await;
            assert!(completed.get());
        })
        .await;
    }
}
