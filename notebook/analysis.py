import marimo

__generated_with = "0.19.8"
app = marimo.App(width="medium")

with app.setup(hide_code=True):
    import math
    import sys
    import json
    import polars as pl
    import altair as alt
    import marimo as mo
    from pathlib import Path
    from pint import UnitRegistry

    ureg = UnitRegistry()

    def is_wasm() -> bool:
        return "pyodide" in sys.modules

    local_tz = "Europe/Warsaw"

    RESOLUTION_BUF_CAP = 1024
    RESOLUTION_MIN_SAMPLES = 10
    RESOLUTION_WINDOW_US = 60_000_000
    LN_2 = math.log(2)


@app.function(hide_code=True)
def load_events(path: str | None = None) -> pl.DataFrame:
    def _ts(n):
        return pl.col(n).str.to_datetime("%+", time_unit="ns", time_zone=local_tz)

    if path is None:
        notebook_loc = mo.notebook_location()
        if notebook_loc is None:
            raise RuntimeError(
                "Notebook location not available (are you importing this module?). Pass `path=`."
            )
        data_path = notebook_loc / "files" / "mouse-sample.ndjson"
    else:
        data_path = Path(path)

    if is_wasm():
        from pyodide.http import open_url  # pyright: ignore[reportMissingImports]
        content = open_url(str(data_path)).read()
        records = [json.loads(line) for line in content.strip().split("\n") if line.strip()]
        raw = pl.DataFrame(records)
    else:
        raw = pl.scan_ndjson(str(data_path)).collect()

    good = (
        raw.lazy()
        .with_columns(collected_at=_ts("collected_at"), rpc_at=_ts("rpc_at"))
        .explode("readings")
        .rename({"readings": "reading_ts"})
        .with_columns(reading_ts=_ts("reading_ts"))
        .collect()
    )
    if not good["collected_at"].is_sorted():
        raise RuntimeError("Expected sorted input, is the file corrupted?")
    return good


@app.cell(hide_code=True)
def _():
    df = load_events("/tmp/new.ndjson")
    return (df,)


@app.cell
def _(df):
    df
    return


@app.cell(hide_code=True)
def _():
    binning_slider = mo.ui.slider(
        debounce=True,
        start=5000,
        stop=100_000,
        step=1000,
        value=50_000,
        show_value=True,
        label="Binning resolution (µs)",
    )
    averaging_slider = mo.ui.slider(
        debounce=True,
        start=1000,
        stop=1_000_000,
        step=100,
        value=200_000,
        show_value=True,
        label="Averaging resolution (µs)",
    )
    return averaging_slider, binning_slider


@app.cell(hide_code=True)
def overall_chart(averaging_slider, binning_slider, df):
    _binning_resolution = binning_slider.value
    _averaging_period = averaging_slider.value
    _df = (
        df.with_columns(
            reading_ts=(
                pl.col("reading_ts") - pl.col("reading_ts").min()
            ).dt.total_microseconds()
        )
        .unique("reading_ts", maintain_order=True)
        .group_by_dynamic(
            "reading_ts",
            period=f"{_averaging_period}i",
            every=f"{_binning_resolution}i"
        )
        .agg(pl.col("reading_ts").count().alias("count"))
        .with_columns(frequency=pl.col("count") * (1 / (_averaging_period * ureg.microsecond)).to(ureg.hertz).magnitude)
    )

    _brush = alt.selection_interval(name="selection", encodings=["x"])

    _chart = (
        alt.Chart(_df)
        .mark_bar()
        .encode(
            x=alt.X("reading_ts:Q").axis(title="Time Since Start (us)"),
            y=alt.Y("frequency:Q").axis(title="Hz"), #.scale(type="log"),
            tooltip=[
                alt.Tooltip("reading_ts:Q", title="Time (us)"),
                alt.Tooltip("frequency:Q", title="Frequency (Hz)"),
            ],
        )
        .add_params(_brush)
    )

    overall_chart = mo.ui.altair_chart(_chart)
    mo.vstack([mo.md("# Raw input"), binning_slider, averaging_slider, overall_chart])
    return


@app.cell(hide_code=True)
def _():
    mo.md(r"""
    # Collection stats
    """)
    return


@app.cell(hide_code=True)
def _(df):
    _res = (
        df.lazy()
        .unique("reading_ts", maintain_order=True)
        .with_columns(
            secs=(
                pl.col("reading_ts") - pl.col("reading_ts").min()
            ).dt.total_microseconds(),
        )
        .select(delta=pl.col("secs") - pl.col("secs").shift(1))
    )
    _min = _res.min().first().collect().item(0, "delta")
    _median = _res.quantile(0.5).collect().item(0, "delta")
    _p99 = _res.quantile(0.99).collect().item(0, "delta")
    _p10 = _res.quantile(0.1).collect().item(0, "delta")

    def _present(delta):
        us = delta * ureg.microsecond
        return rf"""**{us}** between events, or **{round((1 / us).to(ureg.hertz), 2)}**"""


    mo.vstack(
        [
            mo.md(rf"""## Input resolution
    The following stats are inferred from the delay between subsequent recorded events.
    Batched event collection is excluded from this - timestamps are deduplicated.

    Max event resolution: {_present(_min)}  
    p10 resolution is: {_present(_p10)}  
    median resolution is: {_present(_median)}  
    p99 resolution is: {_present(_p99)}

    """)
        ]
    )
    return


@app.cell(hide_code=True)
def _(df):
    _res = df.lazy().select(
        rpc_delay=(
            pl.col("rpc_at") - pl.col("collected_at")
        ).dt.total_microseconds(),
        collection_delay=(
            pl.col("collected_at") - pl.col("reading_ts")
        ).dt.total_microseconds(),
    )
    _max = _res.select("rpc_delay").max().first().collect().item(0, "rpc_delay")
    _median = _res.select("rpc_delay").quantile(0.5).collect().item(0, "rpc_delay")
    _p99 = _res.select("rpc_delay").quantile(0.99).collect().item(0, "rpc_delay")

    _maxc = (
        _res.select("collection_delay")
        .max()
        .first()
        .collect()
        .item(0, "collection_delay")
    )
    _medianc = (
        _res.select("collection_delay")
        .quantile(0.5)
        .collect()
        .item(0, "collection_delay")
    )
    _p99c = (
        _res.select("collection_delay")
        .quantile(0.99)
        .collect()
        .item(0, "collection_delay")
    )


    def _present(delta):
        us = delta * ureg.microsecond
        return rf"""**{us:g#~}**"""


    mo.md(rf"""## Collection delays

    Device -> Collection:

    | stat   | value |
    | :----: | ---: |
    | median | {_present(_medianc)} |
    | p99 | {_present(_p99c)} |
    | max | {_present(_maxc)} |

    Server -> Client:

    | stat   | value |
    | :----: | ---: |
    | median | {_present(_median)} |
    | p99 | {_present(_p99)} |
    | max | {_present(_max)} |

    Combined (summing the previous two stats):

    | stat   | value |
    | :----: | ---: |
    | median | {_present(_median + _medianc)} |
    | p99 | {_present(_p99 + _p99c)} |
    | max | {_present(_max + _maxc)} |

    """)
    return


@app.cell(hide_code=True)
def _():
    mo.vstack([mo.md("# Strategies")])
    return


@app.cell(hide_code=True)
def _():
    fps_range_slider = mo.ui.range_slider(
        start=30,
        stop=240,
        step=5,
        value=[45, 117],
        show_value=True,
        label="Max FPS",
        full_width=True,
    )

    mo.vstack(
        [
            fps_range_slider,
            mo.md(
                r"This should be within your VRR range, minus a slight margin. For example, on a 40-120 FPS VRR range display, I use 45-117 to try and always stay within the VRR window."
            ),
        ]
    )
    return (fps_range_slider,)


@app.cell(hide_code=True)
def _(fps_range_slider):
    min_fps, max_fps = fps_range_slider.value
    return max_fps, min_fps


@app.cell(hide_code=True)
def _():
    mo.md(r"""
    ## Hold & linear decay

    A conservative strategy that jumps to max frame rate after every input, holds it for a set duration and then decays linearly over time.
    """)
    return


@app.cell(hide_code=True)
def _():
    hold_for_slider = mo.ui.slider(
        start=0,
        stop=1000,
        step=5,
        value=40,
        show_value=True,
        label="Hold max FPS for (milliseconds)",
        full_width=True,
    )
    hold_decay_slider = mo.ui.slider(
        start=0,
        stop=100,
        step=1,
        value=15,
        show_value=True,
        label="Decay, in FPS per second",
        full_width=True,
    )
    return hold_decay_slider, hold_for_slider


@app.cell(hide_code=True)
def _(df, hold_decay_slider, hold_for_slider, max_fps, min_fps):
    _hold_for = (hold_for_slider.value * ureg.millisecond).to(ureg.microsecond)
    _hold_decay = (hold_decay_slider.value / ureg.second).to(1 / ureg.microsecond)

    _df = (
        df.lazy()
        .unique("reading_ts", maintain_order=True)
        .select(
            ev_ts=(
                pl.col("reading_ts") - pl.col("reading_ts").min()
            ).dt.total_microseconds()
        )
    )

    _resampled = _df.select(
        pl.arange(pl.col("ev_ts").min(), pl.col("ev_ts").max(), step=1000).alias(
            "reading_ts"
        ),
    )

    _df = (
        _resampled.join_asof(
            _df,
            left_on="reading_ts",
            right_on="ev_ts",
            strategy="backward",
        )
        .with_columns(since_input=pl.col("reading_ts") - pl.col("ev_ts"))
        .with_columns(
            fps_limit=pl.when(pl.col("since_input") < _hold_for.magnitude)
            .then(max_fps)
            .otherwise(
                max_fps
                - (pl.col("since_input") - _hold_for.magnitude)
                * _hold_decay.magnitude,
            )
            .clip(min_fps, max_fps),
        )
        .group_by_dynamic("reading_ts", every="10000i")
        .agg(pl.col("fps_limit").mean())
        .with_columns(reading_ts=pl.col("reading_ts") / 1e6)
    )

    _brush = alt.selection_interval(bind="scales", encodings=["x"])

    _chart = (
        alt.Chart(_df.collect())
        .mark_line()
        .encode(
            x=alt.X("reading_ts:Q").axis(format="~s", title="Time in seconds (s)"),
            y=alt.Y("fps_limit:Q"),
            tooltip=[
                alt.Tooltip("reading_ts:Q", format="~s", title="Time (s)"),
                alt.Tooltip("fps_limit:Q", title="FPS Limit"),
            ],
        )
        .add_params(_brush)
    )

    mo.vstack(
        [
            hold_for_slider,
            hold_decay_slider,
            _chart,
        ]
    )
    return


@app.cell(hide_code=True)
def _():
    mo.md(r"""
    ## Short & Long window Exponential Moving Average

    This uses two windows, each doing a symmetric EWMA. They are blended by taking the max of both windows.
    """)
    return


@app.cell(hide_code=True)
def _():
    half_life_short_slider = mo.ui.slider(
        start=1000,
        stop=1_000_000,
        step=1000,
        value=200_000,
        show_value=True,
        label="Short window Half life (microseconds)",
        full_width=True,
    )
    half_life_long_slider = mo.ui.slider(
        start=1000,
        stop=10_000_000,
        step=1000,
        value=1_000_000,
        show_value=True,
        label="Long window Half life (microseconds)",
        full_width=True,
    )
    return half_life_long_slider, half_life_short_slider


@app.function(hide_code=True)
def compute_ewm_fps(
    events: pl.LazyFrame,
    hl_short_us: int,
    hl_long_us: int,
    min_fps: float,
    max_fps: float,
    tick_us: int = 1000,
) -> pl.LazyFrame:
    df = events.unique("reading_ts", maintain_order=True).select(
        ev_ts=(
            pl.col("reading_ts") - pl.col("reading_ts").min()
        ).dt.total_microseconds(),
    )

    # Rolling p10 delta per event, matching Rust's ResolutionEstimator.p10()
    # (1024-sample ring buffer, min 10 samples for valid output)
    event_stats = (
        df.with_columns(delta=pl.col("ev_ts").diff())
        .filter(pl.col("delta") > 0)
        .with_columns(
            mean_delta=pl.col("delta")
            .rolling_quantile(quantile=0.1, window_size=1024, min_samples=10)
        )
        .select("ev_ts", "mean_delta")
    )

    binned = (
        df.with_columns(tick=(pl.col("ev_ts") // tick_us * tick_us))
        .group_by("tick")
        .agg(x=pl.len())
    )

    grid = df.select(
        pl.arange(
            pl.col("ev_ts").min(), pl.col("ev_ts").max(), step=tick_us
        ).alias("reading_ts"),
    )
    grid = (
        grid.join(
            binned, left_on="reading_ts", right_on="tick", how="left"
        )
        .with_columns(pl.col("x").fill_null(0))
        .join_asof(
            event_stats,
            left_on="reading_ts",
            right_on="ev_ts",
            strategy="backward",
        )
        .with_columns(
            expected=pl.lit(tick_us) / pl.col("mean_delta").fill_null(tick_us)
        )
    )

    return (
        grid.with_columns(
            fps_limit_short=(
                pl.col("x").ewm_mean_by(
                    "reading_ts", half_life=f"{hl_short_us}i"
                )
                / pl.col("expected")
            ).clip(0, 1)
            * (max_fps - min_fps)
            + min_fps,
            fps_limit_long=(
                pl.col("x").ewm_mean_by(
                    "reading_ts", half_life=f"{hl_long_us}i"
                )
                / pl.col("expected")
            ).clip(0, 1)
            * (max_fps - min_fps)
            + min_fps,
        )
        .with_columns(
            fps_limit=pl.max_horizontal(pl.col("fps_limit_short"), pl.col("fps_limit_long"))
        )
    )


@app.cell(hide_code=True)
def _(df, half_life_long_slider, half_life_short_slider, max_fps, min_fps):
    _hl_short_us = half_life_short_slider.value
    _hl_long_us = half_life_long_slider.value

    _df = (
        compute_ewm_fps(df.lazy(), _hl_short_us, _hl_long_us, min_fps, max_fps)
        .group_by_dynamic("reading_ts", every="10000i")
        .agg(
            pl.col("fps_limit").mean(),
            pl.col("fps_limit_short").mean(),
            pl.col("fps_limit_long").mean(),
        )
        .with_columns(reading_ts=pl.col("reading_ts") / 1e6)
        .rename(
            {
                "fps_limit": "FPS Limit",
                "fps_limit_short": "Short window",
                "fps_limit_long": "Long window",
            }
        )
        .unpivot(index="reading_ts", variable_name="metric", value_name="value")
    )

    _brush = alt.selection_interval(bind="scales", encodings=["x"])

    _chart = (
        alt.Chart(_df.collect())
        .mark_line()
        .encode(
            x=alt.X("reading_ts:Q").axis(format="~s", title="Time in seconds (s)"),
            y=alt.Y("value:Q"),
            color=alt.Color("metric:N").scale(scheme="category10"),
            strokeDash=alt.condition(
                alt.datum.metric == "FPS Limit",
                alt.value([6, 2]),
                alt.value([8, 16]),
            ),
            tooltip=[
                alt.Tooltip("reading_ts:Q", format="~s", title="Time (s)"),
                alt.Tooltip("value:Q"),
            ],
        )
    )

    mo.vstack(
        [
            half_life_short_slider,
            half_life_long_slider,
            _chart.add_params(_brush),
        ]
    )
    return


@app.cell(hide_code=True)
def _():
    mo.md(r"""
    ## Asymmetric IIR
    """)
    return


@app.cell(hide_code=True)
def _():
    attack_hl_slider = mo.ui.slider(
        start=1000,
        stop=1_000_000,
        step=1000,
        value=400_000,
        show_value=True,
        label="Attack half life (µs)",
        full_width=True,
    )
    release_hl_slider = mo.ui.slider(
        start=1000,
        stop=10_000_000,
        step=1000,
        value=2_000_000,
        show_value=True,
        label="Release half life (µs)",
        full_width=True,
    )
    return attack_hl_slider, release_hl_slider


@app.class_definition(hide_code=True)
class ResolutionEstimator:
    def __init__(self):
        self.buf: list[int] = []

    def push_events(self, ts: list[int]):
        assert len(ts) > 0
        assert all(ts[i] < ts[i + 1] for i in range(len(ts) - 1)), "must be strictly increasing"
        assert not self.buf or ts[0] > self.buf[-1], "first timestamp must be strictly after buf[-1]"

        cutoff = ts[-1] - RESOLUTION_WINDOW_US
        while self.buf and self.buf[0] < cutoff:
            self.buf.pop(0)

        from bisect import bisect_left
        start = bisect_left(ts, cutoff)
        ts = ts[start:]
        if not ts:
            return

        total = len(self.buf) + len(ts)
        if total > RESOLUTION_BUF_CAP:
            excess = total - RESOLUTION_BUF_CAP
            drain_existing = min(excess, len(self.buf))
            self.buf = self.buf[drain_existing:]
            skip_incoming = excess - drain_existing
            ts = ts[skip_incoming:]

        self.buf.extend(ts)

    def _num_deltas(self) -> int:
        return max(0, len(self.buf) - 1)

    def p10(self) -> float | None:
        if self._num_deltas() < RESOLUTION_MIN_SAMPLES:
            return None
        deltas = sorted(self.buf[i] - self.buf[i - 1] for i in range(1, len(self.buf)))
        k = len(deltas) // 10
        return float(deltas[k])

    def mean_delta(self) -> float | None:
        if self._num_deltas() < RESOLUTION_MIN_SAMPLES:
            return None
        return (self.buf[-1] - self.buf[0]) / self._num_deltas()


@app.class_definition(hide_code=True)
class DeviceEwm:
    def __init__(self, attack_hl_us: float, release_hl_us: float):
        self.attack_hl_us = attack_hl_us
        self.release_hl_us = release_hl_us
        self.y = 0.0
        self.pending_events = 0
        self.last_tick_us: int | None = None
        self.last_event_us: int | None = None
        self.resolution = ResolutionEstimator()

    def observe_batch(self, timestamps_us: list[int]):
        if self.last_event_us is not None:
            from bisect import bisect_right
            start = bisect_right(timestamps_us, self.last_event_us)
            skipped = timestamps_us[start:]
        else:
            skipped = timestamps_us
        if not skipped:
            return
        self.last_event_us = skipped[-1]
        self.pending_events += len(skipped)
        self.resolution.push_events(skipped)

    def compute_fps_detailed(
        self, now_us: int, min_fps: float, max_fps: float
    ) -> dict[str, float | int | None]:
        events = self.pending_events
        x = float(events)
        self.pending_events = 0

        min_detail = {
            "fps_limit": min_fps,
            "norm": 0.0,
            "y": 0.0,
            "expected": 0.0,
            "mean_delta": self.resolution.mean_delta(),
            "events": events,
        }

        if self.last_event_us is None:
            self.last_tick_us = now_us
            return min_detail

        if self.last_tick_us is not None:
            dt = float(max(now_us - self.last_tick_us, 1))
        else:
            dt = 0.0
        self.last_tick_us = now_us

        mean_delta = self.resolution.p10()
        input_res = mean_delta if mean_delta is not None else max(dt, 1.0)
        expected = dt / input_res

        idle_us = float(now_us - self.last_event_us)
        in_hold = idle_us <= input_res * 2.0

        if dt > 0.0:
            if x > 0.0 or in_hold:
                alpha = 1.0 - math.exp(-LN_2 * dt / self.attack_hl_us)
                self.y += alpha * (1.0 - self.y)
            else:
                alpha = 1.0 - math.exp(-LN_2 * dt / self.release_hl_us)
                self.y += alpha * (0.0 - self.y)

        norm = max(0.0, min(1.0, self.y))
        fps_limit = norm * (max_fps - min_fps) + min_fps

        return {
            "fps_limit": fps_limit,
            "norm": norm,
            "y": self.y,
            "expected": expected,
            "mean_delta": mean_delta,
            "events": events,
        }


@app.function(hide_code=True)
def compute_iir_fps(
    events: pl.LazyFrame,
    attack_hl_us: float,
    release_hl_us: float,
    min_fps: float,
    max_fps: float,
    tick_us: int = 1000,
) -> pl.DataFrame:
    timestamps = (
        events.unique("reading_ts", maintain_order=True)
        .select(
            ev_ts=(
                pl.col("reading_ts") - pl.col("reading_ts").min()
            ).dt.total_microseconds(),
        )
        .collect()
        .get_column("ev_ts")
        .to_list()
    )

    ewm = DeviceEwm(attack_hl_us, release_hl_us)
    last_event = timestamps[-1]
    tick = tick_us

    rows = []
    ev_idx = 0
    t = tick
    while t <= last_event + tick:
        start = ev_idx
        while ev_idx < len(timestamps) and timestamps[ev_idx] <= t:
            ev_idx += 1
        if start < ev_idx:
            ewm.observe_batch(timestamps[start:ev_idx])
        detail = ewm.compute_fps_detailed(t, min_fps, max_fps)
        rows.append({"time_us": t, **detail})
        t += tick

    return pl.DataFrame(rows, schema={
        "time_us": pl.Int64,
        "fps_limit": pl.Float64,
        "norm": pl.Float64,
        "y": pl.Float64,
        "expected": pl.Float64,
        "mean_delta": pl.Float64,
        "events": pl.UInt32,
    })


@app.cell(hide_code=True)
def _(attack_hl_slider, df, max_fps, min_fps, release_hl_slider):
    iir_df = compute_iir_fps(
        df.lazy(),
        attack_hl_slider.value,
        release_hl_slider.value,
        min_fps,
        max_fps,
    )
    return (iir_df,)


@app.cell(hide_code=True)
def _(attack_hl_slider, iir_df, release_hl_slider):
    _df = (
        iir_df.select("time_us", "fps_limit", "norm")
        .group_by_dynamic("time_us", every="10000i")
        .agg(pl.col("fps_limit").mean())
        .with_columns(time_s=pl.col("time_us") / 1e6)
    )

    _brush = alt.selection_interval(bind="scales", encodings=["x"])

    _chart = (
        alt.Chart(_df)
        .mark_line()
        .encode(
            x=alt.X("time_s:Q").axis(format="~s", title="Time in seconds (s)"),
            y=alt.Y("fps_limit:Q"),
        )
        .add_params(_brush)
    )

    mo.vstack(
        [
            attack_hl_slider,
            release_hl_slider,
            _chart,
        ]
    )
    return


@app.cell(hide_code=True)
def _():
    mo.md(r"""
    # Supplemental
    """)
    return


@app.cell(hide_code=True)
def _(df):
    _chart = df.select(
        rpc_delay=(
            pl.col("rpc_at") - pl.col("collected_at")
        ).dt.total_microseconds(),
        collection_delay=(
            pl.col("collected_at") - pl.col("reading_ts")
        ).dt.total_microseconds(),
    ).plot.scatter(x="rpc_delay:Q", y="collection_delay:Q")

    mo.vstack([
        mo.md("## Rpc & collection delay scatter plot"),
        mo.ui.altair_chart(_chart)
    ])
    return


if __name__ == "__main__":
    app.run()
