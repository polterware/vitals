//! The stream: a hello, then a metrics frame on every tick, with the replies to stdin's
//! commands and rate-limited warnings in between. Fast fields go out on every frame;
//! processes every 2 s, GPUs every 3 s, and disks and thermals every 10 s, only on the frame
//! that collected them. It knows no platform: a [`Collector`] reads the host.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use crate::commands::{self, KillFn};
use crate::hub::Hub;
use crate::metrics::{
    CpuSample, DiskSample, NetSample, ProcessSample, cpu_usage, disk_rates, net_rates,
    rank_processes,
};
use crate::protocol::{
    self, Core, Disk, DiskIo, Gpu, Hello, Memory, Metrics, Net, Pressure, Process, Thermal, Warn,
};

pub const PROC_EVERY: Duration = Duration::from_secs(2);
pub const GPU_EVERY: Duration = Duration::from_secs(3);
pub const SLOW_EVERY: Duration = Duration::from_secs(10);
pub const WARN_EVERY: Duration = Duration::from_secs(10);
pub const PROCESS_TOP: usize = 25;
/// A command line longer than this ends the stream, as the Go agent's scanner did.
const MAX_COMMAND: usize = 1 << 20;
const WARN_MAX: usize = 200;

/// One fast sample: each reading, or why it could not be read.
pub struct FastRaw {
    /// Wall time, Unix milliseconds.
    pub when_ms: i64,
    /// Time since the agent started.
    pub mono: Duration,
    pub cpus: Result<Vec<CpuSample>, String>,
    pub memory: Result<Memory, String>,
    pub nets: Result<Vec<NetSample>, String>,
    pub load: Result<[f64; 3], String>,
    pub uptime: Result<f64, String>,
    pub disks: Result<Vec<DiskSample>, String>,
    /// `None` where the host has no pressure stall information.
    pub pressure: Result<Option<Pressure>, String>,
}

impl FastRaw {
    pub fn new(start: Instant) -> Self {
        let when_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis() as i64);
        Self {
            when_ms,
            mono: start.elapsed(),
            cpus: Ok(Vec::new()),
            memory: Ok(Memory::default()),
            nets: Ok(Vec::new()),
            load: Ok([0.0; 3]),
            uptime: Ok(0.0),
            disks: Ok(Vec::new()),
            pressure: Ok(None),
        }
    }
}

/// What a host reads. Kills run on stdin's thread, apart from sampling, so they are a
/// separate [`KillFn`].
pub trait Collector {
    fn hello(&mut self) -> Hello;
    fn sample_fast(&mut self) -> FastRaw;
    fn sample_procs(&mut self, mem_total_kb: u64) -> Result<Vec<ProcessSample>, String>;
    fn sample_disks(&mut self) -> Result<Vec<Disk>, String>;
    fn sample_thermals(&mut self) -> Result<Vec<Thermal>, String>;
    fn sample_gpus(&mut self) -> Result<Vec<Gpu>, String>;
    fn gpu_enabled(&self) -> bool;
}

/// What the stream waits on.
pub enum Event {
    Tick,
    /// Stdin reached its end (the app closed the channel).
    InputClosed,
    /// The writer stopped: stdout broke, or the hub closed.
    OutputDone(io::Result<()>),
}

/// One warning per collector every [`WARN_EVERY`].
#[derive(Default)]
struct Limiter {
    last: HashMap<String, Duration>,
}

impl Limiter {
    fn allow(&mut self, key: &str, now: Duration) -> bool {
        if let Some(prev) = self.last.get(key)
            && now.saturating_sub(*prev) < WARN_EVERY
        {
            return false;
        }
        self.last.insert(key.to_string(), now);
        true
    }
}

fn warn_line(collector: &str, message: &str) -> Vec<u8> {
    let mut end = message.len().min(WARN_MAX);
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    protocol::line(&Warn {
        kind: "warn",
        collector: collector.to_string(),
        message: message[..end].to_string(),
    })
}

/// Runs the stream until stdin closes (`Ok`), stdout breaks (its error), or every sender of
/// `events` is gone. `events` is the sender the stream's own threads report on; `receiver`
/// also hears the ticks.
pub fn serve<R, W, C>(
    input: R,
    mut output: W,
    mut collector: C,
    kill: KillFn,
    events: Sender<Event>,
    receiver: Receiver<Event>,
) -> io::Result<()>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
    C: Collector,
{
    let hub = Arc::new(Hub::new());
    {
        let hub = hub.clone();
        let events = events.clone();
        std::thread::spawn(move || {
            let result = hub.run(&mut output);
            let _ = events.send(Event::OutputDone(result));
        });
    }
    hub.push_ctrl(protocol::line(&collector.hello()));
    {
        let hub = hub.clone();
        std::thread::spawn(move || {
            read_commands(input, &hub, &kill);
            let _ = events.send(Event::InputClosed);
        });
    }

    let mut limiter = Limiter::default();
    let mut state = RateState::default();
    loop {
        match receiver.recv() {
            Ok(Event::Tick) => {
                let mut warn = |name: &str, message: &str, now: Duration| {
                    if !message.is_empty() && limiter.allow(name, now) {
                        hub.push_ctrl(warn_line(name, message));
                    }
                };
                let frame = emit(&mut collector, &mut state, &mut warn);
                hub.push_metric(protocol::line(&frame));
            }
            Ok(Event::InputClosed) => {
                hub.close();
                // A blocked stdout write must not keep the agent alive after stdin closes.
                return finish(&receiver, Some(Duration::from_secs(1)));
            }
            Ok(Event::OutputDone(result)) => return result,
            Err(_) => {
                hub.close();
                return finish(&receiver, None);
            }
        }
    }
}

/// Waits for the writer to finish what the closed hub holds.
fn finish(receiver: &Receiver<Event>, limit: Option<Duration>) -> io::Result<()> {
    let deadline = limit.map(|limit| Instant::now() + limit);
    loop {
        let event = match deadline {
            Some(deadline) => {
                match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(event) => event,
                    Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {
                        return Ok(());
                    }
                }
            }
            None => match receiver.recv() {
                Ok(event) => event,
                Err(_) => return Ok(()),
            },
        };
        if let Event::OutputDone(result) = event {
            return result;
        }
    }
}

fn read_commands(input: impl Read, hub: &Hub, kill: &KillFn) {
    let mut reader = BufReader::with_capacity(64 * 1024, input);
    let mut limiter = Limiter::default();
    let start = Instant::now();
    let mut line = Vec::new();
    loop {
        line.clear();
        match (&mut reader)
            .take(MAX_COMMAND as u64 + 1)
            .read_until(b'\n', &mut line)
        {
            Ok(0) | Err(_) => return,
            Ok(_) if line.len() > MAX_COMMAND => return,
            Ok(_) => {}
        }
        let trimmed = line.trim_ascii();
        if trimmed.is_empty() {
            continue;
        }
        let (reply, warn) = commands::dispatch(trimmed, kill.as_ref());
        if let Some(message) = warn
            && limiter.allow("stdin", start.elapsed())
        {
            hub.push_ctrl(warn_line("stdin", message));
        }
        if let Some(reply) = reply {
            hub.push_ctrl(reply);
        }
    }
}

/// What the stream carries from one tick to the next: the last values (a failed read keeps
/// them), the samples rates are taken against, and when each slow field was last collected.
#[derive(Default)]
struct RateState {
    prev_cpu: Vec<CpuSample>,
    cpu: f64,
    cores: Vec<Core>,
    memory: Memory,
    load: [f64; 3],
    uptime: f64,
    pressure: Option<Pressure>,

    net_at: Option<Duration>,
    prev_net: Vec<NetSample>,
    net: Vec<Net>,

    disk_at: Option<Duration>,
    prev_disk: Vec<DiskSample>,
    disk_io: Vec<DiskIo>,

    procs_at: Option<Duration>,
    prev_cpu_ns: HashMap<i64, u64>,
    disks_at: Option<Duration>,
    thermals_at: Option<Duration>,
    gpus_at: Option<Duration>,
}

/// True when a slow field was never collected, or `every` has passed since.
fn due(last: Option<Duration>, now: Duration, every: Duration) -> bool {
    match last {
        None => true,
        Some(last) => now >= last && now - last >= every,
    }
}

fn elapsed_since(last: Option<Duration>, now: Duration) -> f64 {
    match last {
        Some(last) if now > last => (now - last).as_secs_f64(),
        _ => 0.0,
    }
}

fn emit<C: Collector>(
    collector: &mut C,
    state: &mut RateState,
    warn: &mut dyn FnMut(&str, &str, Duration),
) -> Metrics {
    use protocol::finite;
    let raw = collector.sample_fast();
    let now = raw.mono;

    match raw.cpus {
        Err(error) => warn("cpu", &error, now),
        Ok(cpus) => {
            let (total, cores) = cpu_usage(&state.prev_cpu, &cpus);
            state.prev_cpu = cpus;
            state.cpu = finite(total);
            state.cores = cores
                .into_iter()
                .map(|core| Core {
                    name: core.name,
                    usage_percent: finite(core.usage_percent),
                })
                .collect();
        }
    }
    match raw.memory {
        Err(error) => warn("mem", &error, now),
        Ok(memory) => state.memory = memory,
    }
    match raw.nets {
        Err(error) => warn("net", &error, now),
        Ok(nets) => {
            let rates = net_rates(&state.prev_net, &nets, elapsed_since(state.net_at, now));
            state.prev_net = nets;
            state.net = rates
                .into_iter()
                .map(|rate| Net {
                    name: rate.name,
                    rx_bytes_per_sec: finite(rate.rx_bytes_per_sec),
                    tx_bytes_per_sec: finite(rate.tx_bytes_per_sec),
                })
                .collect();
            state.net_at = Some(now);
        }
    }
    match raw.disks {
        Err(error) => warn("diskio", &error, now),
        Ok(disks) => {
            let rates = disk_rates(&state.prev_disk, &disks, elapsed_since(state.disk_at, now));
            state.prev_disk = disks;
            state.disk_io = rates
                .into_iter()
                .map(|rate| DiskIo {
                    device: rate.device,
                    read_bytes_per_sec: finite(rate.read_bytes_per_sec),
                    write_bytes_per_sec: finite(rate.write_bytes_per_sec),
                    util_percent: finite(rate.util_percent),
                })
                .collect();
            state.disk_at = Some(now);
        }
    }
    match raw.load {
        Err(error) => warn("load", &error, now),
        Ok(load) => state.load = load.map(finite),
    }
    match raw.uptime {
        Err(error) => warn("uptime", &error, now),
        Ok(uptime) => state.uptime = finite(uptime),
    }
    match raw.pressure {
        Err(error) => warn("pressure", &error, now),
        Ok(pressure) => {
            state.pressure = pressure.map(|pressure| Pressure {
                cpu: finite(pressure.cpu),
                memory: finite(pressure.memory),
                io: finite(pressure.io),
            });
        }
    }

    let mut frame = Metrics {
        kind: "metrics",
        ts_unix_ms: raw.when_ms,
        mono_ns: now.as_nanos() as i64,
        cpu_total_percent: state.cpu,
        cpu_cores: state.cores.clone(),
        load: state.load,
        uptime_seconds: state.uptime,
        memory: state.memory,
        network: state.net.clone(),
        disk_io: state.disk_io.clone(),
        pressure: state.pressure,
        disks: None,
        processes: None,
        thermals: None,
        gpus: None,
    };

    if due(state.procs_at, now, PROC_EVERY) {
        match collector.sample_procs(state.memory.total_kb) {
            Err(error) => warn("proc", &error, now),
            Ok(samples) => {
                let (rows, next) = rank_processes(
                    samples,
                    &state.prev_cpu_ns,
                    elapsed_since(state.procs_at, now),
                    state.memory.total_kb.saturating_mul(1024),
                    PROCESS_TOP,
                );
                state.prev_cpu_ns = next;
                frame.processes = Some(
                    rows.into_iter()
                        .map(|row| Process {
                            pid: row.pid,
                            user: row.user,
                            cpu_percent: finite(row.cpu_percent),
                            mem_percent: finite(row.mem_percent),
                            command: row.command,
                        })
                        .collect(),
                );
                state.procs_at = Some(now);
            }
        }
    }
    if due(state.disks_at, now, SLOW_EVERY) {
        match collector.sample_disks() {
            Err(error) => warn("disk", &error, now),
            Ok(disks) => {
                frame.disks = Some(disks);
                state.disks_at = Some(now);
            }
        }
    }
    if due(state.thermals_at, now, SLOW_EVERY) {
        match collector.sample_thermals() {
            Err(error) => warn("thermal", &error, now),
            Ok(zones) => {
                frame.thermals = Some(zones);
                state.thermals_at = Some(now);
            }
        }
    }
    if collector.gpu_enabled() && due(state.gpus_at, now, GPU_EVERY) {
        match collector.sample_gpus() {
            Err(error) => warn("gpu", &error, now),
            Ok(gpus) => {
                frame.gpus = Some(gpus);
                state.gpus_at = Some(now);
            }
        }
    }
    frame
}

/// The CPU sample macOS and Windows build: `cpu` (the sum) first, then one per core.
pub fn with_total(cores: Vec<CpuSample>) -> Vec<CpuSample> {
    let mut total = CpuSample {
        name: "cpu".into(),
        ..CpuSample::default()
    };
    for core in &cores {
        total.user += core.user;
        total.nice += core.nice;
        total.system += core.system;
        total.idle += core.idle;
    }
    let mut out = Vec::with_capacity(cores.len() + 1);
    out.push(total);
    out.extend(cores);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::io::{PipeReader, PipeWriter};
    use std::sync::mpsc;

    struct Fake {
        fast: Vec<FastRaw>,
        next: usize,
        gpu: bool,
    }

    impl Collector for Fake {
        fn hello(&mut self) -> Hello {
            Hello::new("box".into(), "6.8.0".into(), 2)
        }

        fn sample_fast(&mut self) -> FastRaw {
            let index = self.next.min(self.fast.len() - 1);
            self.next += 1;
            let raw = &self.fast[index];
            FastRaw {
                when_ms: raw.when_ms,
                mono: raw.mono,
                cpus: raw.cpus.clone(),
                memory: raw.memory.clone(),
                nets: raw.nets.clone(),
                load: raw.load.clone(),
                uptime: raw.uptime.clone(),
                disks: raw.disks.clone(),
                pressure: raw.pressure.clone(),
            }
        }

        fn sample_procs(&mut self, _: u64) -> Result<Vec<ProcessSample>, String> {
            Ok(vec![ProcessSample {
                pid: 42,
                user: "www".into(),
                command: "nginx".into(),
                cpu_ns: 1_000_000_000,
                rss_bytes: 4096,
            }])
        }

        fn sample_disks(&mut self) -> Result<Vec<Disk>, String> {
            Ok(vec![Disk {
                filesystem: "/dev/sda1".into(),
                size_kb: 100,
                used_kb: 40,
                avail_kb: 60,
                use_percent: 40.0,
                mount_point: "/".into(),
            }])
        }

        fn sample_thermals(&mut self) -> Result<Vec<Thermal>, String> {
            Ok(vec![Thermal {
                id: "thermal_zone0".into(),
                celsius: 45.0,
            }])
        }

        fn sample_gpus(&mut self) -> Result<Vec<Gpu>, String> {
            Ok(vec![Gpu {
                index: 0,
                name: "T4".into(),
                ..Gpu::default()
            }])
        }

        fn gpu_enabled(&self) -> bool {
            self.gpu
        }
    }

    fn fast(user: u64, idle: u64, mono: Duration) -> FastRaw {
        let cpu = |name: &str| CpuSample {
            name: name.into(),
            user,
            idle,
            ..CpuSample::default()
        };
        FastRaw {
            when_ms: 1_700_000_000_000,
            mono,
            cpus: Ok(vec![cpu("cpu"), cpu("cpu0")]),
            memory: Ok(Memory {
                total_kb: 1000,
                available_kb: 400,
                used_kb: 600,
                ..Memory::default()
            }),
            nets: Ok(vec![NetSample {
                name: "eth0".into(),
                rx: 1000 + mono.as_secs() * 100,
                tx: 50,
            }]),
            load: Ok([0.1, 0.2, 0.3]),
            uptime: Ok(10.0),
            disks: Ok(vec![DiskSample {
                device: "sda".into(),
                read_sectors: 10,
                write_sectors: 4,
                io_time_ms: 10,
            }]),
            pressure: Ok(Some(Pressure {
                cpu: 1.0,
                memory: 2.0,
                io: 3.0,
            })),
        }
    }

    struct Running {
        ticks: Sender<Event>,
        input: PipeWriter,
        output: BufReader<PipeReader>,
        done: mpsc::Receiver<io::Result<()>>,
    }

    impl Running {
        fn frame(&mut self) -> Value {
            let (tx, rx) = mpsc::channel();
            let mut line = String::new();
            let reader = &mut self.output;
            std::thread::scope(|scope| {
                scope.spawn(|| {
                    reader.read_line(&mut line).unwrap();
                    let _ = tx.send(());
                });
                rx.recv_timeout(Duration::from_secs(2))
                    .expect("a frame within 2s");
            });
            serde_json::from_str(&line).unwrap()
        }
    }

    fn start(fake: Fake) -> Running {
        let (input_rx, input) = std::io::pipe().unwrap();
        let (output, output_tx) = std::io::pipe().unwrap();
        let (events, receiver) = mpsc::channel();
        let ticks = events.clone();
        let kill: KillFn =
            Arc::new(|pid, signal| commands::authorize_kill(pid, 10, Ok(10), signal));
        let (done_tx, done) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = done_tx.send(serve(input_rx, output_tx, fake, kill, events, receiver));
        });
        Running {
            ticks,
            input,
            output: BufReader::new(output),
            done,
        }
    }

    #[test]
    fn slow_fields_come_only_when_collected_again() {
        let mut run = start(Fake {
            fast: vec![
                fast(0, 100, Duration::ZERO),
                fast(50, 150, Duration::from_secs(1)),
                fast(50, 150, Duration::from_secs(2)),
            ],
            next: 0,
            gpu: true,
        });
        let hello = run.frame();
        assert_eq!(
            (hello["type"].as_str(), hello["v"].as_u64()),
            (Some("hello"), Some(1))
        );

        run.ticks.send(Event::Tick).unwrap();
        let first = run.frame();
        for key in [
            "disks",
            "processes",
            "thermals",
            "gpus",
            "pressure",
            "diskIo",
        ] {
            assert!(
                first.get(key).is_some(),
                "the first frame has {key}: {first}"
            );
        }
        assert_eq!(first["cpuTotalPercent"], 0.0);

        run.ticks.send(Event::Tick).unwrap();
        let second = run.frame();
        for key in ["disks", "processes", "thermals", "gpus"] {
            assert!(second.get(key).is_none(), "the second frame left out {key}");
        }
        assert!(second.get("pressure").is_some(), "pressure is fast");
        assert_eq!(second["cpuTotalPercent"], 50.0);
        assert_eq!(second["network"][0]["rxBytesPerSec"], 100.0);

        run.input.write_all(b"{\"cmd\":\"ping\"}\n").unwrap();
        assert_eq!(run.frame()["type"], "pong");
        run.input
            .write_all(b"{\"cmd\":\"kill\",\"pid\":42,\"signal\":\"TERM\",\"id\":\"k\"}\n")
            .unwrap();
        let killed = run.frame();
        assert_eq!(
            (killed["type"].as_str(), killed["ok"].as_bool()),
            (Some("kill_result"), Some(true))
        );

        drop(run.input);
        let result = run
            .done
            .recv_timeout(Duration::from_secs(2))
            .expect("the stream ends when stdin closes");
        assert!(result.is_ok());
    }

    #[test]
    fn warnings_are_rate_limited() {
        let failing = |mono| FastRaw {
            cpus: Err("stat unreadable".into()),
            ..FastRaw {
                when_ms: 1,
                ..fast(0, 0, mono)
            }
        };
        let mut run = start(Fake {
            fast: vec![
                failing(Duration::ZERO),
                failing(Duration::from_secs(1)),
                failing(Duration::from_secs(11)),
            ],
            next: 0,
            gpu: false,
        });
        run.frame();
        let mut warns = 0;
        for _ in 0..3 {
            run.ticks.send(Event::Tick).unwrap();
            loop {
                let frame = run.frame();
                match frame["type"].as_str() {
                    Some("warn") => {
                        assert_eq!(frame["collector"], "cpu");
                        warns += 1;
                    }
                    Some("metrics") => break,
                    _ => panic!("unexpected {frame}"),
                }
            }
        }
        assert_eq!(warns, 2);
        run.input.write_all(b"not json\n").unwrap();
        let warn = run.frame();
        assert_eq!(
            (warn["collector"].as_str(), warn["message"].as_str()),
            (Some("stdin"), Some("invalid command"))
        );
    }

    #[test]
    fn long_warnings_are_cut_on_a_character() {
        let line = warn_line("cpu", &"é".repeat(150));
        let frame: Value = serde_json::from_slice(&line).unwrap();
        assert_eq!(frame["message"].as_str().unwrap().len(), 200);
    }

    #[test]
    fn the_total_leads_the_cores() {
        let core = |name: &str, user| CpuSample {
            name: name.into(),
            user,
            idle: 1,
            ..CpuSample::default()
        };
        let samples = with_total(vec![core("cpu0", 2), core("cpu1", 3)]);
        assert_eq!(samples[0].name, "cpu");
        assert_eq!((samples[0].user, samples[0].idle), (5, 2));
        assert_eq!(samples.len(), 3);
    }
}
