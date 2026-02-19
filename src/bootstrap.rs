use capnp::capability::Promise;

use crate::mangochill_capnp;

pub struct MangoChillImpl {
    fps_limiter: mangochill_capnp::fps_limiter::Client,
    raw_events: Option<mangochill_capnp::raw_events::Client>,
}

impl MangoChillImpl {
    pub fn new(
        fps_limiter: mangochill_capnp::fps_limiter::Client,
        raw_events: Option<mangochill_capnp::raw_events::Client>,
    ) -> Self {
        Self {
            fps_limiter,
            raw_events,
        }
    }
}

impl mangochill_capnp::mango_chill::Server for MangoChillImpl {
    fn fps_limiter(
        &mut self,
        _: mangochill_capnp::mango_chill::FpsLimiterParams,
        mut ret: mangochill_capnp::mango_chill::FpsLimiterResults,
    ) -> Promise<(), capnp::Error> {
        ret.get().set_service(self.fps_limiter.clone());
        Promise::ok(())
    }

    fn raw_events(
        &mut self,
        _: mangochill_capnp::mango_chill::RawEventsParams,
        mut ret: mangochill_capnp::mango_chill::RawEventsResults,
    ) -> Promise<(), capnp::Error> {
        match &self.raw_events {
            Some(client) => {
                ret.get().set_service(client.clone());
                Promise::ok(())
            }
            None => Promise::err(capnp::Error::failed(
                "raw events not enabled (server needs --raw flag)".to_string(),
            )),
        }
    }
}
