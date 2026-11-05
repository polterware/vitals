use std::collections::HashMap;

use super::*;
use crate::protocol::Memory;

macro_rules! fixture {
    ($name:literal) => {
        include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/", $name))
    };
}

fn near(got: f64, want: f64) {
    assert!((got - want).abs() < 1e-6, "got {got} want {want}");
}

#[test]
fn cpu_deltas() {
    let prev = parse_proc_stat(fixture!("stat_prev.txt"));
    let curr = parse_proc_stat(fixture!("stat_curr.txt"));
    let reset = parse_proc_stat(fixture!("stat_reset.txt"));
    assert_eq!(cpu_count(&prev), 2);
    for (name, before, after, total, core0) in [
        ("half busy", &prev[..], &curr[..], 50.0, 50.0),
        ("counter reset", &prev[..], &reset[..], 0.0, 0.0),
        ("first sample", &[][..], &curr[..], 0.0, 0.0),
    ] {
        let (got, cores) = cpu_usage(before, after);
        near(got, total);
        assert_eq!(cores.len(), 2, "{name}");
        near(cores[0].usage_percent, core0);
        assert_eq!(
            (cores[0].name.as_str(), cores[1].name.as_str()),
            ("cpu0", "cpu1")
        );
    }
}

#[test]
fn meminfo() {
    assert_eq!(
        parse_meminfo(fixture!("meminfo.txt")),
        Memory {
            total_kb: 8_000_000,
            available_kb: 3_000_000,
            used_kb: 5_000_000,
            swap_total_kb: 1_000_000,
            swap_free_kb: 250_000,
            swap_used_kb: 750_000,
        }
    );
}

#[test]
fn network_rates() {
    let keep = |samples: Vec<NetSample>| -> Vec<NetSample> {
        samples
            .into_iter()
            .filter(|sample| {
                let name = sample.name.as_str();
                keep_iface(name, matches!(name, "eth0" | "eth1" | "tailscale0"))
            })
            .collect()
    };
    let prev = keep(parse_net_dev(fixture!("netdev_prev.txt")));
    let curr = keep(parse_net_dev(fixture!("netdev_curr.txt")));
    let reset = keep(parse_net_dev(fixture!("netdev_reset.txt")));
    assert_eq!(prev.len(), 2);
    assert_eq!(prev[0].name, "eth0");
    for (name, after, elapsed, want) in [
        ("delta", &curr, 2.0, [(2000.0, 0.0), (0.0, 1000.0)]),
        ("counter reset", &reset, 2.0, [(50.0, 25.0), (25.0, 12.5)]),
        ("zero elapsed", &curr, 0.0, [(0.0, 0.0), (0.0, 0.0)]),
    ] {
        let got = net_rates(&prev, after, elapsed);
        assert_eq!(got.len(), 2, "{name}");
        assert_eq!(
            (got[0].name.as_str(), got[1].name.as_str()),
            ("eth0", "eth1")
        );
        for (rate, (rx, tx)) in got.iter().zip(want) {
            near(rate.rx_bytes_per_sec, rx);
            near(rate.tx_bytes_per_sec, tx);
        }
    }
    assert!(
        !keep_iface("tailscale0", false),
        "a tun without a device link is dropped"
    );
    assert!(
        keep_iface("tailscale0", true),
        "a device link is physical whatever the name"
    );
    assert!(!keep_iface("docker0", true));
}

#[test]
fn disk_io_rates() {
    let whole = |samples: Vec<DiskSample>| -> Vec<DiskSample> {
        samples
            .into_iter()
            .filter(|sample| looks_like_whole_disk(&sample.device))
            .collect()
    };
    let prev = whole(parse_diskstats(fixture!("diskstats_prev.txt")));
    let curr = whole(parse_diskstats(fixture!("diskstats_curr.txt")));
    let reset = whole(parse_diskstats(fixture!("diskstats_reset.txt")));
    let devices: Vec<&str> = prev.iter().map(|sample| sample.device.as_str()).collect();
    assert_eq!(devices, ["sda", "nvme0n1"]);
    let sector = SECTOR_SIZE as f64;
    for (name, after, elapsed, read, write, util) in [
        ("delta", &curr, 1.0, 2000.0 * sector, 0.0, 50.0),
        (
            "counter reset",
            &reset,
            1.0,
            10.0 * sector,
            4.0 * sector,
            25.0,
        ),
        ("zero elapsed", &curr, 0.0, 0.0, 0.0, 0.0),
    ] {
        let got = disk_rates(&prev, after, elapsed);
        assert_eq!(got.len(), 2, "{name}");
        assert_eq!(got[0].device, "sda");
        near(got[0].read_bytes_per_sec, read);
        near(got[0].write_bytes_per_sec, write);
        near(got[0].util_percent, util);
    }
    // Utilization stops at 100 when io time runs past the interval.
    let sample = |io_time_ms| DiskSample {
        device: "sda".into(),
        io_time_ms,
        ..DiskSample::default()
    };
    near(
        disk_rates(&[sample(0)], &[sample(5000)], 1.0)[0].util_percent,
        100.0,
    );
    assert!(looks_like_whole_disk("mmcblk0"));
    assert!(!looks_like_whole_disk("mmcblk0p1"));
    assert!(!looks_like_whole_disk("sda1"));
    assert!(!looks_like_whole_disk("loop0"));
}

#[test]
fn pressure() {
    near(parse_psi_avg10(fixture!("pressure_cpu.txt")).unwrap(), 1.25);
    near(
        parse_psi_avg10(fixture!("pressure_memory.txt")).unwrap(),
        2.5,
    );
    near(parse_psi_avg10(fixture!("pressure_io.txt")).unwrap(), 0.0);
    assert_eq!(parse_psi_avg10("full avg10=9.00 avg60=0 total=1\n"), None);
}

#[test]
fn process_deltas() {
    const TICK_NS: u64 = 10_000_000; // USER_HZ 100
    const PAGE: u64 = 4096;
    let first = parse_pid_stat(fixture!("pid_stat.txt")).unwrap();
    let rewound = parse_pid_stat(fixture!("pid_stat_reset.txt")).unwrap();
    assert_eq!(
        (
            first.pid,
            first.command.as_str(),
            first.ticks(),
            first.rss_pages
        ),
        (42, "nginx worker", 3000, 7)
    );
    assert_eq!(parse_status_uid(fixture!("pid_status.txt")), Ok(1000));

    let sample = |stat: &PidStat| ProcessSample {
        pid: stat.pid,
        user: "www".into(),
        command: stat.command.clone(),
        cpu_ns: stat.ticks() * TICK_NS,
        rss_bytes: stat.rss_pages * PAGE,
    };
    let base = first.ticks() * TICK_NS;
    for (name, stat, prev, cpu) in [
        ("first sight", &rewound, HashMap::new(), 0.0),
        (
            "delta",
            &first,
            HashMap::from([(42, 2500 * TICK_NS)]),
            500.0,
        ),
        (
            "counter reset",
            &rewound,
            HashMap::from([(42, base)]),
            150.0,
        ),
    ] {
        let input = sample(stat);
        let cpu_ns = input.cpu_ns;
        let (rows, next) = rank_processes(vec![input], &prev, 1.0, 8_000_000 * 1024, 25);
        assert_eq!(rows.len(), 1, "{name}");
        near(rows[0].cpu_percent, cpu);
        assert_eq!(next[&42], cpu_ns, "{name}");
        if name == "delta" {
            near(
                rows[0].mem_percent,
                (7 * PAGE) as f64 / (8_000_000.0 * 1024.0) * 100.0,
            );
        }
    }

    let many: Vec<ProcessSample> = (0..30)
        .map(|index| ProcessSample {
            pid: index + 2,
            user: "u".into(),
            command: String::new(),
            cpu_ns: (100 + index as u64) * TICK_NS,
            rss_bytes: 0,
        })
        .collect();
    let prev: HashMap<i64, u64> = many
        .iter()
        .map(|sample| (sample.pid, 100 * TICK_NS))
        .collect();
    let (rows, next) = rank_processes(many, &prev, 1.0, 1, 25);
    assert_eq!((rows.len(), next.len()), (25, 30));
    assert_eq!(rows[0].pid, 31, "the busiest process comes first");
    assert_eq!(rows[0].command, "?", "a nameless process still shows");
    assert!(parse_pid_stat("garbage").is_err());
    assert!(parse_pid_stat("(x) S 1").is_err());
}

#[test]
fn mounts_and_usage() {
    let text = "/dev/sda1 / ext4 rw 0 0\n\
                proc /proc proc rw 0 0\n\
                tmpfs /run tmpfs rw 0 0\n\
                /dev/sdb1 /mnt/My\\040Disk xfs rw 0 0\n\
                overlay /var/lib/docker/overlay2 overlay rw 0 0\n";
    let mounts = parse_mounts(text);
    assert_eq!(mounts.len(), 3);
    assert_eq!(mounts[1].mount_point, "/mnt/My Disk");
    assert_eq!(mounts[1].fs_type, "xfs");
    assert_eq!(mounts[2].fs_type, "overlay");
    let used = usage(1000, 400, 500, 4096);
    assert_eq!(
        (used.size_kb, used.used_kb, used.avail_kb),
        (4000, 2400, 2000)
    );
    near(used.use_percent, 60.0);
    assert_eq!(usage(1, 1, 1, 0), DiskUsage::default());
}

#[test]
fn load_uptime_thermal_gpu() {
    assert_eq!(
        parse_loadavg("0.15 0.10 0.05 1/100 12\n"),
        Some([0.15, 0.10, 0.05])
    );
    assert_eq!(parse_loadavg("0.15 x"), None);
    assert_eq!(parse_uptime("12345.5 999.0\n"), Some(12345.5));
    near(milli_celsius("45123\n").unwrap(), 45.123);
    let gpus = parse_nvidia_smi("0, NVIDIA T4, 55, 10, 100, 8192\n1, A10, [N/A], 3, 1, 2\n");
    assert_eq!(gpus.len(), 2);
    assert_eq!(gpus[0].name, "NVIDIA T4");
    assert_eq!(gpus[0].memory_total_mb, 8192.0);
    assert_eq!(gpus[1].temperature_c, 0.0);
}
