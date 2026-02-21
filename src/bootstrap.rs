use std::rc::Rc;

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
    async fn fps_limiter(
        self: Rc<Self>,
        _: mangochill_capnp::mango_chill::FpsLimiterParams,
        mut ret: mangochill_capnp::mango_chill::FpsLimiterResults,
    ) -> Result<(), capnp::Error> {
        ret.get().set_service(self.fps_limiter.clone());
        Ok(())
    }

    async fn raw_events(
        self: Rc<Self>,
        _: mangochill_capnp::mango_chill::RawEventsParams,
        mut ret: mangochill_capnp::mango_chill::RawEventsResults,
    ) -> Result<(), capnp::Error> {
        match &self.raw_events {
            Some(client) => {
                ret.get().set_service(client.clone());
                Ok(())
            }
            None => Err(capnp::Error::failed(
                "raw events not enabled (server needs --raw flag)".to_string(),
            )),
        }
    }
}
