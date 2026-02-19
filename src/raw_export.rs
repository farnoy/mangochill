use capnp::capability::Promise;
use capnp_rpc::pry;
use chrono::{DateTime, Utc};
use mangochill::{LogRecord, connect_rpc};
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio::task::{LocalSet, spawn_local};
use tokio::time::timeout;

use clap::Parser;
use clap_verbosity_flag::Verbosity;
use log::{error, info, warn};

use mangochill::{
    mangochill_capnp::{
        self,
        poll_receiver::{ReceiveParams, ReceiveResults},
    },
    socket_path,
};

#[derive(Parser, Debug)]
#[command(name = "raw_export", version, about = "Mangochill raw event exporter")]
struct Cli {
    #[command(flatten)]
    verbosity: Verbosity,

    /// Unix socket path to the RPC server
    #[arg(short = 's', long)]
    rpc_socket: Option<PathBuf>,

    /// Path to record raw events to, in newline-delimited JSON
    #[arg(short, long)]
    output: PathBuf,
}

struct Receiver {
    tx: mpsc::UnboundedSender<String>,
}

impl mangochill_capnp::poll_receiver::Server for Receiver {
    fn receive(&mut self, params: ReceiveParams, _: ReceiveResults) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let now = Utc::now();
        let mono_to_real_us = now.timestamp_micros() - mangochill::monotonic_us();
        let collected_at = pry!(p.get_collected_at());
        let collected_at = mono_to_real(mono_to_real_us, &collected_at);
        let readings = pry!(p.get_readings());
        let readings = readings
            .into_iter()
            .map(|r| mono_to_real(mono_to_real_us, &r))
            .collect();
        let record = LogRecord {
            collected_at,
            rpc_at: now,
            readings,
        };
        let serialized = serde_json::to_string(&record).unwrap();
        if let Err(e) = self.tx.send(serialized) {
            warn!("Failed to send event to writer: {}", e);
        }
        Promise::ok(())
    }
}

fn mono_to_real(offset_us: i64, r: &mangochill_capnp::timestamp::Reader<'_>) -> DateTime<Utc> {
    let us = r.get_seconds() * 1_000_000 + r.get_microseconds() as i64 + offset_us;
    DateTime::from_timestamp_micros(us).unwrap()
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    mangochill::init_logging(cli.verbosity.log_level_filter());

    let rpc_socket = socket_path(cli.rpc_socket);
    let (rpc_system, mango_chill) = connect_rpc(&rpc_socket).await?;

    let set = LocalSet::new();
    set.run_until(async {
        let rpc_disconnector = rpc_system.get_disconnector();
        let rpc_bg = spawn_local(rpc_system);

        // Get RawEvents service via bootstrap
        let raw_events = match timeout(
            Duration::from_secs(5),
            mango_chill.raw_events_request().send().promise,
        )
        .await
        {
            Ok(Ok(resp)) => resp.get().unwrap().get_service().unwrap(),
            Ok(Err(e)) => {
                error!("Failed to get RawEvents service (is server running with --raw?): {e}");
                return Ok(Ok(()));
            }
            Err(_) => {
                error!("Timed out getting RawEvents service");
                return Ok(Ok(()));
            }
        };

        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let poller = capnp_rpc::new_client(Receiver { tx });

        let mut req = raw_events.register_request();
        req.get().set_receiver(poller);
        let Ok(resp) = timeout(Duration::from_secs(5), req.send().promise).await else {
            error!("Failed to connect to the server - is it running?");
            return Ok(Ok(()));
        };
        let resp = resp.unwrap();
        let _subscription = resp.get().unwrap().get_subscription().unwrap();

        let output_file = tokio::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&cli.output)
            .await
            .unwrap();
        let mut writer = tokio::io::BufWriter::new(output_file);

        let mut writer_task = async |rx: &mut mpsc::UnboundedReceiver<String>| {
            while let Some(record) = rx.recv().await {
                writer.write_all(record.as_bytes()).await.unwrap();
                writer.write_all(b"\n").await.unwrap();
            }
        };

        tokio::select! {
            _ = mangochill::termination_signal() => {}
            _ = writer_task(&mut rx) => {}
        }

        // Drop our poll_receiver and initiate RPC disconnect
        drop(_subscription);
        rpc_disconnector.await.unwrap();

        // close and drain the writer queue
        rx.close();
        writer_task(&mut rx).await;

        // flush and close the output stream
        writer.flush().await.unwrap();
        drop(writer);

        info!("exiting");

        rpc_bg.await
    })
    .await?
    .unwrap();

    Ok(())
}
