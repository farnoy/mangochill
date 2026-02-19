use mangochill::ewm::DeviceEwm;
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub struct Replay {
    tick: i64,
    attack_hl_us: f64,
    release_hl_us: f64,
    min_fps: f64,
    max_fps: f64,
    ewm: DeviceEwm,
    next_t: i64,
    scratch: Vec<i64>,
    col_fps_limit: Vec<f64>,
    col_norm: Vec<f64>,
    col_events: Vec<f64>,
}

impl Default for Replay {
    fn default() -> Self {
        Self::new()
    }
}

#[wasm_bindgen]
impl Replay {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Replay {
        Replay {
            tick: 0,
            attack_hl_us: 0.0,
            release_hl_us: 0.0,
            min_fps: 0.0,
            max_fps: 0.0,
            ewm: DeviceEwm::new(0.0, 0.0),
            next_t: 0,
            scratch: Vec::new(),
            col_fps_limit: Vec::new(),
            col_norm: Vec::new(),
            col_events: Vec::new(),
        }
    }

    pub fn reset(
        &mut self,
        attack_half_life_ms: f64,
        release_half_life_ms: f64,
        min_fps: f64,
        max_fps: f64,
        tick_us: f64,
    ) {
        self.attack_hl_us = attack_half_life_ms * 1000.0;
        self.release_hl_us = release_half_life_ms * 1000.0;
        self.min_fps = min_fps;
        self.max_fps = max_fps;
        self.tick = tick_us as i64;
        self.ewm = DeviceEwm::new(self.attack_hl_us, self.release_hl_us);
        self.next_t = 0;
        self.col_fps_limit.clear();
        self.col_norm.clear();
        self.col_events.clear();
    }

    pub fn run(&mut self, new_timestamps_us: &[f64], now_us: f64) -> Result<JsValue, JsValue> {
        self.scratch.clear();
        self.scratch
            .extend(new_timestamps_us.iter().map(|&t| t as i64));

        let horizon = now_us as i64;

        if self.next_t == 0 && !self.scratch.is_empty() {
            self.next_t = self.tick;
        }

        let mut ev_idx = 0;

        while self.next_t > 0 && self.next_t <= horizon {
            let start = ev_idx;
            while ev_idx < self.scratch.len() && self.scratch[ev_idx] <= self.next_t {
                ev_idx += 1;
            }
            if start < ev_idx {
                self.ewm.observe_batch(&self.scratch[start..ev_idx]);
            }
            let d = self
                .ewm
                .compute_fps_detailed(self.next_t, self.min_fps, self.max_fps);
            self.col_fps_limit.push(d.fps_limit);
            self.col_norm.push(d.expected);
            self.col_events.push(d.events as f64);
            self.next_t += self.tick;
        }

        if ev_idx < self.scratch.len() {
            self.ewm.observe_batch(&self.scratch[ev_idx..]);
        }

        self.result()
    }

    fn result(&self) -> Result<JsValue, JsValue> {
        if self.col_fps_limit.is_empty() {
            return Ok(JsValue::NULL);
        }
        let obj = js_sys::Object::new();
        let set = |k: &str, v: &[f64]| {
            js_sys::Reflect::set(&obj, &JsValue::from_str(k), &js_sys::Float64Array::from(v))
        };
        set("fps_limit", &self.col_fps_limit)?;
        set("norm", &self.col_norm)?;
        set("events", &self.col_events)?;
        Ok(obj.into())
    }
}
