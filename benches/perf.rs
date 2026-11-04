#![allow(clippy::cast_precision_loss)]

#[allow(dead_code)]
#[path = "../tests/support/fake_herdr.rs"]
mod fake_herdr;

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use fake_herdr::FakeHerdr;
use nix::pty::{Winsize, openpty};
use nix::sys::resource::{UsageWho, getrusage};
use nix::sys::time::TimeValLike;
use serde_json::{Value, json};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const RUNS: usize = 120;
const WARMUP: usize = 3;
const BINARY: &str = env!("CARGO_BIN_EXE_herdr-triage");

struct Running {
    child: Child,
    kill_group: bool,
}
impl Drop for Running {
    fn drop(&mut self) {
        if self.kill_group
            && self.child.try_wait().ok().flatten().is_none()
            && let Ok(pid) = i32::try_from(self.child.id())
        {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn command(fake: &FakeHerdr, executable: &Path) -> Command {
    let mut cmd = Command::new(executable);
    cmd.env_clear()
        .env("HERDR_SOCKET_PATH", &fake.socket)
        .env("HERDR_PLUGIN_ID", "ar.triage")
        .env("HERDR_PLUGIN_STATE_DIR", state_dir(fake))
        .env("HERDR_PLUGIN_CONFIG_DIR", fake.dir.path().join("config"))
        .env("TERM", "xterm-256color")
        .env("TZ", "UTC");
    cmd
}

/// SQLite's commit cost is an fsync, which is free on the tmpfs `/tmp` common on Linux; keep the
/// store on the real disk beside `target/` so the numbers include it. The socket stays in `/tmp`
/// because Unix socket paths are length-limited.
fn state_dir(fake: &FakeHerdr) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target/bench/state")
        .join(fake.dir.path().file_name().unwrap_or_default())
}

fn run(fake: &FakeHerdr, arg: &str) -> Result<()> {
    let output = command(fake, Path::new(BINARY)).arg(arg).output()?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned().into());
    }
    Ok(())
}

fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let dest = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &dest)?;
        } else {
            fs::copy(entry.path(), dest)?;
        }
    }
    Ok(())
}

fn worker() -> Result<()> {
    nix::unistd::setsid()?;
    let args: Vec<String> = std::env::args().collect();
    let output = Path::new(&args[2]);
    let started_ns =
        nix::time::clock_gettime(nix::time::ClockId::CLOCK_MONOTONIC)?.num_nanoseconds();
    let started = Instant::now();
    let mut child = Running {
        child: Command::new(BINARY).args(&args[3..]).spawn()?,
        kill_group: false,
    };
    let status = child.child.wait()?;
    let wall = started.elapsed().as_secs_f64() * 1000.0;
    if !status.success() {
        return Err(format!("measured binary exited {status}").into());
    }
    let usage = getrusage(UsageWho::RUSAGE_CHILDREN)?;
    let rss = usage.max_rss();
    #[cfg(target_os = "macos")]
    let rss = rss / 1024;
    fs::write(
        output,
        serde_json::to_vec(&json!({
            "wall_ms":wall, "started_ns":started_ns,
            "user_ms":usage.user_time().num_microseconds() as f64 / 1000.0,
            "sys_ms":usage.system_time().num_microseconds() as f64 / 1000.0,
            "peak_rss_kib":rss
        }))?,
    )?;
    Ok(())
}

fn wait(child: &mut Running) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.child.try_wait()? {
            return if status.success() {
                Ok(())
            } else {
                Err(format!("worker exited {status}").into())
            };
        }
        if Instant::now() >= deadline {
            return Err("worker timeout".into());
        }
        thread::sleep(Duration::from_millis(1));
    }
}

struct Terminal {
    writer: File,
    chunks: Receiver<Vec<u8>>,
    pending: Vec<u8>,
    bytes: usize,
}
impl Terminal {
    fn open(cmd: &mut Command) -> Result<Self> {
        let pty = openpty(
            Some(&Winsize {
                ws_row: 40,
                ws_col: 120,
                ws_xpixel: 0,
                ws_ypixel: 0,
            }),
            None,
        )?;
        let slave = File::from(pty.slave);
        cmd.stdin(slave.try_clone()?)
            .stdout(slave.try_clone()?)
            .stderr(slave);
        let mut reader = File::from(pty.master);
        let writer = reader.try_clone()?;
        let (tx, chunks) = mpsc::channel();
        thread::spawn(move || {
            let mut buffer = [0; 16384];
            while let Ok(n) = reader.read(&mut buffer) {
                if n == 0 || tx.send(buffer[..n].to_vec()).is_err() {
                    break;
                }
            }
        });
        Ok(Terminal {
            writer,
            chunks,
            pending: Vec::new(),
            bytes: 0,
        })
    }

    fn frame(&mut self) -> Result<()> {
        // Ratatui 0.29 emits Hide at the end of each non-search frame.
        let marker = b"\x1b[?25l";
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(pos) = self.pending.windows(marker.len()).position(|w| w == marker) {
                self.pending.drain(..pos + marker.len());
                return Ok(());
            }
            let chunk = self
                .chunks
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))?;
            self.bytes += chunk.len();
            self.pending.extend(chunk);
        }
    }
}

fn sample(fake: &FakeHerdr, scenario: &str, idle: bool) -> Result<Value> {
    let report = fake.dir.path().join("sample.json");
    let mut cmd = command(fake, &std::env::current_exe()?);
    cmd.args([
        "--worker",
        report.to_str().ok_or("non-UTF8 temp path")?,
        scenario,
    ]);
    let mut terminal = None;
    if scenario == "picker" {
        terminal = Some(Terminal::open(&mut cmd)?);
    } else {
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
    }
    let mut child = Running {
        child: cmd.spawn()?,
        kill_group: true,
    };
    drop(cmd);
    let mut extra = json!({});
    if let Some(mut terminal) = terminal {
        terminal.frame()?;
        extra["first_frame_ns"] =
            json!(nix::time::clock_gettime(nix::time::ClockId::CLOCK_MONOTONIC)?.num_nanoseconds());
        let key = Instant::now();
        terminal.writer.write_all(b"j")?;
        terminal.frame()?;
        extra["keypress_ms"] = json!(key.elapsed().as_secs_f64() * 1000.0);
        if idle {
            fake.requests
                .lock()
                .map_err(|_| "poisoned requests")?
                .clear();
            let bytes = terminal.bytes;
            let started = Instant::now();
            let deadline = started + Duration::from_secs(5);
            while Instant::now() < deadline {
                match terminal
                    .chunks
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                {
                    Ok(chunk) => terminal.bytes += chunk.len(),
                    Err(mpsc::RecvTimeoutError::Timeout) => break,
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        return Err("picker PTY closed during idle".into());
                    }
                }
            }
            let seconds = started.elapsed().as_secs_f64();
            let counts = fake.counts();
            let refreshes = counts.get("agent.list").copied().unwrap_or(0);
            let per_refresh: BTreeMap<_, _> = counts
                .iter()
                .map(|(k, v)| (k, *v as f64 / refreshes.max(1) as f64))
                .collect();
            extra["idle"] = json!({"seconds":seconds, "bytes_per_second":(terminal.bytes-bytes) as f64/seconds,
                "refreshes":refreshes, "rpc_counts":counts, "rpc_per_refresh":per_refresh,
                "rpcs_per_second":counts.values().sum::<usize>() as f64/seconds});
        }
        terminal.writer.write_all(b"q")?;
        wait(&mut child)?;
    } else {
        wait(&mut child)?;
    }
    let mut result: Value = serde_json::from_slice(&fs::read(report)?)?;
    if let Some(end) = extra
        .as_object_mut()
        .ok_or("extra object missing")?
        .remove("first_frame_ns")
    {
        extra["first_frame_ms"] = json!(
            (end.as_i64().ok_or("frame timestamp missing")?
                - result["started_ns"]
                    .as_i64()
                    .ok_or("start timestamp missing")?) as f64
                / 1_000_000.0
        );
    }
    result
        .as_object_mut()
        .ok_or("result object missing")?
        .remove("started_ns");
    result
        .as_object_mut()
        .ok_or("worker JSON is not object")?
        .extend(extra.as_object().ok_or("extra is not object")?.clone());
    Ok(result)
}

fn quantile(mut values: Vec<f64>, percent: usize) -> f64 {
    values.sort_by(f64::total_cmp);
    if percent == 50 && values.len().is_multiple_of(2) {
        return f64::midpoint(values[values.len() / 2 - 1], values[values.len() / 2]);
    }
    values[(values.len() * percent).div_ceil(100).saturating_sub(1)]
}

fn percentile(samples: &[Value], field: &str, percent: usize) -> f64 {
    quantile(
        samples.iter().filter_map(|v| v[field].as_f64()).collect(),
        percent,
    )
}

fn summarize(samples: &[Value], counts: &BTreeMap<String, usize>) -> Value {
    let cpu: Vec<f64> = samples
        .iter()
        .map(|s| s["user_ms"].as_f64().unwrap_or(0.0) + s["sys_ms"].as_f64().unwrap_or(0.0))
        .collect();
    let mut result = json!({"samples":samples.len(), "runs":samples, "wall_ms_median":percentile(samples,"wall_ms",50),
        "wall_ms_p90":percentile(samples,"wall_ms",90), "user_ms_median":percentile(samples,"user_ms",50),
        "sys_ms_median":percentile(samples,"sys_ms",50), "cpu_ms_median":quantile(cpu,50),
        "peak_rss_kib":samples.iter().filter_map(|s| s["peak_rss_kib"].as_i64()).max(), "rpc_counts":counts});
    if samples[0].get("first_frame_ms").is_some() {
        for field in ["first_frame_ms", "keypress_ms"] {
            result[format!("{field}_median")] = json!(percentile(samples, field, 50));
            result[format!("{field}_p90")] = json!(percentile(samples, field, 90));
        }
    }
    result
}

#[cfg(target_os = "linux")]
fn sleeper(fake: &FakeHerdr) -> Result<Value> {
    let until = chrono::Utc::now().timestamp() + 3600;
    let mut child = Running {
        child: command(fake, Path::new(BINARY))
            .args(["wake-at", &until.to_string()])
            .spawn()?,
        kill_group: false,
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    let base = std::path::PathBuf::from(format!("/proc/{}", child.child.id()));
    loop {
        let status = fs::read_to_string(base.join("status"))?;
        if status.lines().any(|s| s.starts_with("State:\tS")) {
            let rss = status
                .lines()
                .find(|s| s.starts_with("VmRSS:"))
                .and_then(|s| s.split_whitespace().nth(1))
                .ok_or("no VmRSS")?
                .parse::<u64>()?;
            child.child.kill()?;
            child.child.wait()?;
            return Ok(json!({"rss_kib":rss}));
        }
        if Instant::now() > deadline {
            return Err("wake-at did not sleep".into());
        }
        thread::sleep(Duration::from_millis(1));
    }
}
#[cfg(not(target_os = "linux"))]
fn sleeper(_: &FakeHerdr) -> Result<Value> {
    Ok(json!({"skipped":"sleeping RSS requires Linux /proc"}))
}

fn fixture(live: usize, closed: usize) -> Result<FakeHerdr> {
    let fake = FakeHerdr::sized(live + closed, 3);
    let all_agents = fake.agents.lock().map_err(|_| "poisoned agents")?.clone();
    *fake.agents.lock().map_err(|_| "poisoned agents")? = all_agents[live..].to_vec();
    run(&fake, "sync")?;
    *fake.agents.lock().map_err(|_| "poisoned agents")? = all_agents[..live].to_vec();
    // Generation changes bypass the product's exit grace and its detached wakers.
    let generation = fs::metadata(&fake.socket)?;
    loop {
        fs::set_permissions(&fake.socket, fs::Permissions::from_mode(0o600))?;
        let current = fs::metadata(&fake.socket)?;
        if (current.ctime(), current.ctime_nsec()) != (generation.ctime(), generation.ctime_nsec())
        {
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }
    for workspace in fake
        .workspaces
        .lock()
        .map_err(|_| "poisoned workspaces")?
        .iter_mut()
    {
        let count = all_agents[..live]
            .iter()
            .filter(|a| a["workspace_id"] == workspace["workspace_id"])
            .count();
        workspace["pane_count"] = json!(count);
        workspace["tab_count"] = json!(count);
    }
    run(&fake, "sync")?;
    Ok(fake)
}

fn bench_size(live: usize, closed: usize) -> Result<Value> {
    let fake = fixture(live, closed)?;
    let snapshot = fake.dir.path().join("snapshot");
    copy_tree(&state_dir(&fake), &snapshot)?;
    let agents = fake.agents.lock().map_err(|_| "poisoned agents")?.clone();
    let restore = || -> Result<()> {
        fs::remove_dir_all(state_dir(&fake))?;
        copy_tree(&snapshot, &state_dir(&fake))?;
        fake.agents
            .lock()
            .map_err(|_| "poisoned agents")?
            .clone_from(&agents);
        fake.requests
            .lock()
            .map_err(|_| "poisoned requests")?
            .clear();
        Ok(())
    };
    let mut result = json!({"live_agents":live,"closed_threads":closed,"workspaces":3});
    let scenarios = [
        "sync_unchanged",
        "sync_changed",
        "next",
        "open",
        "startup",
        "picker",
    ];
    let mut samples = vec![Vec::new(); scenarios.len()];
    let mut expected = vec![None; scenarios.len()];
    for i in 0..RUNS + WARMUP {
        for (index, scenario) in scenarios.iter().copied().enumerate() {
            let arg = if scenario.starts_with("sync_") {
                "sync"
            } else {
                scenario
            };
            restore()?;
            if scenario == "sync_changed" {
                let mut agents = fake.agents.lock().map_err(|_| "poisoned agents")?;
                agents[0]["state_change_seq"] = json!(2);
                agents[0]["agent_status"] = json!("blocked");
            }
            let sample = sample(&fake, arg, false)?;
            let counts = fake.counts();
            if let Some(expected) = &expected[index] {
                if expected != &counts {
                    return Err(format!(
                        "non-deterministic RPCs in {scenario}: {expected:?} vs {counts:?}"
                    )
                    .into());
                }
            } else {
                expected[index] = Some(counts);
            }
            if i >= WARMUP {
                samples[index].push(sample);
            }
        }
    }
    for (index, scenario) in scenarios.iter().copied().enumerate() {
        let summary = summarize(
            &samples[index],
            expected[index].as_ref().ok_or("no samples")?,
        );
        if scenario == "picker" {
            restore()?;
            let idle_sample = sample(&fake, "picker", true)?;
            let mut idle = idle_sample["idle"].clone();
            let total_cpu = idle_sample["user_ms"].as_f64().ok_or("no user CPU")?
                + idle_sample["sys_ms"].as_f64().ok_or("no system CPU")?;
            idle["cpu_seconds_per_second"] = json!(
                ((total_cpu - summary["cpu_ms_median"].as_f64().ok_or("no baseline CPU")?)
                    .max(0.0)
                    / 1000.0)
                    / idle["seconds"].as_f64().ok_or("no seconds")?
            );
            idle["cpu_method"] =
                json!("total child CPU minus median no-idle picker CPU (estimate)");
            result["picker_idle"] = idle;
        }
        result[scenario] = summary;
    }
    result["wake_at"] = sleeper(&fake)?;
    Ok(result)
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).is_some_and(|s| s == "--worker") {
        return worker();
    }
    if !args.iter().any(|s| s == "--bench") {
        return Ok(());
    }
    let started = Instant::now();
    let _ = fs::remove_dir_all(Path::new(env!("CARGO_MANIFEST_DIR")).join("target/bench/state"));
    let mut result = json!({"schema_version":1,"binary_bytes":fs::metadata(BINARY)?.len(),
        "os":std::env::consts::OS,"arch":std::env::consts::ARCH,"warmup":WARMUP,
        "fixture_statuses":["done","blocked","working","idle"],"pty_columns":120,"pty_rows":40,"sampling":"round-robin, restored per sample",
        "peak_rss_method":"child rusage; Linux includes inherited helper RSS floor (~2.6 MiB)",
        "sizes":{}});
    println!("RPC counts are deterministic and the primary signal; timings are host-dependent.");
    println!(
        "size     scenario           median ms    p90 ms    CPU ms    peak KiB  RPCs by method"
    );
    for (name, live, closed) in [("typical", 12, 60), ("heavy", 80, 600)] {
        let size = bench_size(live, closed)?;
        for scenario in [
            "sync_unchanged",
            "sync_changed",
            "next",
            "open",
            "startup",
            "picker",
        ] {
            let row = &size[scenario];
            println!(
                "{name:8} {scenario:18} {:9.3} {:9.3} {:9.3} {:11}  {}",
                row["wall_ms_median"].as_f64().unwrap_or(0.0),
                row["wall_ms_p90"].as_f64().unwrap_or(0.0),
                row["cpu_ms_median"].as_f64().unwrap_or(0.0),
                row["peak_rss_kib"].as_i64().unwrap_or(0),
                row["rpc_counts"]
            );
        }
        println!(
            "{name} picker first-frame median/p90 ms: {:.3}/{:.3}; j-redraw: {:.3}/{:.3}",
            size["picker"]["first_frame_ms_median"]
                .as_f64()
                .unwrap_or(0.0),
            size["picker"]["first_frame_ms_p90"].as_f64().unwrap_or(0.0),
            size["picker"]["keypress_ms_median"].as_f64().unwrap_or(0.0),
            size["picker"]["keypress_ms_p90"].as_f64().unwrap_or(0.0)
        );
        println!(
            "{name} picker idle (5 s): CPU {:.5} s/s (estimate), RPC {:.3}/s, PTY {:.1} bytes/s; {} refreshes, RPC/refresh {}",
            size["picker_idle"]["cpu_seconds_per_second"]
                .as_f64()
                .unwrap_or(0.0),
            size["picker_idle"]["rpcs_per_second"]
                .as_f64()
                .unwrap_or(0.0),
            size["picker_idle"]["bytes_per_second"]
                .as_f64()
                .unwrap_or(0.0),
            size["picker_idle"]["refreshes"],
            size["picker_idle"]["rpc_per_refresh"]
        );
        println!("{name} sleeping wake-at: {}", size["wake_at"]);
        result["sizes"][name] = size;
    }
    result["elapsed_seconds"] = json!(started.elapsed().as_secs_f64());
    let output = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/bench/latest.json");
    fs::create_dir_all(output.parent().ok_or("no output parent")?)?;
    fs::write(&output, serde_json::to_vec_pretty(&result)?)?;
    println!(
        "Release binary: {} bytes; elapsed {:.2} s; JSON: {}",
        result["binary_bytes"],
        started.elapsed().as_secs_f64(),
        output.display()
    );
    Ok(())
}
