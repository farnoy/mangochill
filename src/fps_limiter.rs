use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use log::{info, warn};
use std::collections::HashMap;
use tokio::sync::{Notify, broadcast};
use tokio::task::{spawn_local, yield_now};
use tokio::time::{MissedTickBehavior, interval};
use tokio_util::sync::CancellationToken;
use tracing::trace_span;

use crate::ewm::DeviceEwm;
use crate::mangochill_capnp;

struct FpsSubscriber {
    min_fps: f64,
    max_fps: f64,
    attack_hl_us: f64,
    release_hl_us: f64,
    handle: mangochill_capnp::fps_receiver::Client,
    devices: HashMap<u16, DeviceEwm>,
}

pub struct FpsSubscribers {
    map: HashMap<u32, FpsSubscriber>,
    late_poll_interrupt_tx: broadcast::Sender<()>,
    late_poll_interrupt_rx: broadcast::Receiver<()>,
}

impl Default for FpsSubscribers {
    fn default() -> Self {
        Self::new()
    }
}

impl FpsSubscribers {
    pub fn new() -> Self {
        let (tx, rx) = broadcast::channel(8);
        Self {
            map: Default::default(),
            late_poll_interrupt_tx: tx,
            late_poll_interrupt_rx: rx,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn interrupt_receiver(&self) -> broadcast::Receiver<()> {
        self.late_poll_interrupt_rx.resubscribe()
    }
}

pub struct FpsLimiterImpl {
    next_id: RefCell<u32>,
    subscribers: Rc<RefCell<FpsSubscribers>>,
    ct: CancellationToken,
    notify: Rc<Notify>,
}

impl FpsLimiterImpl {
    pub fn new(
        subscribers: Rc<RefCell<FpsSubscribers>>,
        ct: CancellationToken,
        notify: Rc<Notify>,
    ) -> Self {
        Self {
            next_id: RefCell::new(0),
            subscribers,
            ct,
            notify,
        }
    }
}

impl mangochill_capnp::fps_limiter::Server for FpsLimiterImpl {
    async fn register(
        self: Rc<Self>,
        params: mangochill_capnp::fps_limiter::RegisterParams,
        mut ret: mangochill_capnp::fps_limiter::RegisterResults,
    ) -> Result<(), capnp::Error> {
        let p = params.get()?;
        let frequency_hz = p.get_frequency_hz();
        if !(1..=1000).contains(&frequency_hz) {
            return Err(capnp::Error::failed(
                "frequencyHz must be within 1-1000".to_string(),
            ));
        }
        let min_fps = p.get_min_fps();
        let max_fps = p.get_max_fps();
        if min_fps >= max_fps || max_fps == 0 {
            return Err(capnp::Error::failed("invalid fps range".to_string()));
        }
        let short_hl = p.get_attack_half_life_microseconds();
        let long_hl = p.get_release_half_life_microseconds();
        if short_hl == 0 || long_hl == 0 {
            return Err(capnp::Error::failed("half-lives must be > 0".to_string()));
        }
        let handle = p.get_receiver()?;

        let id = {
            let mut cell = self.next_id.borrow_mut();
            let i = *cell;
            *cell += 1;
            i
        };

        self.subscribers.borrow_mut().map.insert(
            id,
            FpsSubscriber {
                min_fps: min_fps as f64,
                max_fps: max_fps as f64,
                attack_hl_us: short_hl as f64,
                release_hl_us: long_hl as f64,
                handle: handle.clone(),
                devices: HashMap::new(),
            },
        );

        self.notify.notify_one();

        let child_ct = self.ct.child_token();
        let subs = Rc::clone(&self.subscribers);
        let late_poll_interrupt = self.subscribers.borrow().late_poll_interrupt_tx.clone();
        spawn_local(tick_loop(
            id,
            frequency_hz,
            subs,
            child_ct,
            late_poll_interrupt,
        ));

        let disconnector = capnp_rpc::new_client(Disconnector {
            id,
            subscribers: Rc::clone(&self.subscribers),
            notify: Rc::clone(&self.notify),
        });
        ret.get().set_subscription(disconnector);

        info!(
            "fps subscriber {id} registered: {frequency_hz}Hz, fps {min_fps}-{max_fps}, attack/release {short_hl}/{long_hl}µs"
        );
        Ok(())
    }
}

async fn tick_loop(
    id: u32,
    frequency_hz: u16,
    subscribers: Rc<RefCell<FpsSubscribers>>,
    ct: CancellationToken,
    late_poll_sender: broadcast::Sender<()>,
) {
    let period = Duration::from_micros(1_000_000 / frequency_hz as u64);
    let mut ticker = interval(period);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    ct.run_until_cancelled(async {
        loop {
            ticker.tick().await;

            let span = trace_span!("tick_loop::late_poll_begin", id, frequency_hz);
            let g = span.enter();

            late_poll_sender
                .send(())
                .expect("failed to send late poll request");

            drop(g);

            // NOTE: confirmed with tracy that this in fact schedules watch_device()
            //       to do their late polling
            yield_now().await;

            let span = trace_span!("tick_loop::late_poll_resume", id, frequency_hz);
            let g = span.enter();

            let (fps, handle) = {
                let mut subs = subscribers.borrow_mut();
                let Some(sub) = subs.map.get_mut(&id) else {
                    break;
                };

                let now_us = crate::monotonic_us();
                let mut max_fps = sub.min_fps;
                for ewm in sub.devices.values_mut() {
                    let fps = ewm.compute_fps(now_us, sub.min_fps, sub.max_fps);
                    if fps > max_fps {
                        max_fps = fps;
                    }
                }

                (max_fps as f32, sub.handle.clone())
            };

            drop(g);

            let mut req = handle.receive_request();
            req.get().set_fps_limit(fps);
            tracing::trace!(name: "sending reply", id, fps);
            if let Err(e) = req.send().promise.await {
                warn!("fps tick send failed for subscriber {id}: {e}");
                subscribers.borrow_mut().map.remove(&id);
                break;
            }
        }
    })
    .await;

    info!("tick loop {id} stopped");
}

pub fn feed_events(subscribers: &mut FpsSubscribers, device_id: u16, timestamps_us: &[i64]) {
    for sub in subscribers.map.values_mut() {
        let ewm = sub
            .devices
            .entry(device_id)
            .or_insert_with(|| DeviceEwm::new(sub.attack_hl_us, sub.release_hl_us));
        ewm.observe_batch(timestamps_us);
    }
}

struct Disconnector {
    id: u32,
    subscribers: Rc<RefCell<FpsSubscribers>>,
    notify: Rc<Notify>,
}

impl mangochill_capnp::subscription::Server for Disconnector {}
impl Drop for Disconnector {
    fn drop(&mut self) {
        info!("fps subscriber {} disconnected", self.id);
        self.subscribers.borrow_mut().map.remove(&self.id);
        self.notify.notify_one();
    }
}
