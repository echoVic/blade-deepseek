use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use orca_file_search::{
    SearchMode, SearchPhase, SearchSession, SearchSessionOptions, SearchSnapshot, SessionGeneration,
};

const COMPLETED_RSS_LIMIT: usize = 512 * 1024 * 1024;
const PEAK_RSS_LIMIT: usize = 768 * 1024 * 1024;

fn main() {
    let mut path_count = 1_000_000usize;
    let mut assert_slo = false;
    let mut args = std::env::args().skip(1);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--paths" => {
                path_count = args
                    .next()
                    .expect("--paths requires a value")
                    .parse()
                    .expect("--paths must be an integer");
            }
            "--assert-slo" => assert_slo = true,
            other => panic!("unknown argument: {other}"),
        }
    }

    let root = tempfile_dir();
    let baseline_rss = current_rss_bytes().unwrap_or(0);
    let peak_rss = Arc::new(AtomicUsize::new(baseline_rss));
    let monitor_stop = Arc::new(AtomicBool::new(false));
    let monitor_peak = peak_rss.clone();
    let monitor_flag = monitor_stop.clone();
    let monitor = thread::spawn(move || {
        while !monitor_flag.load(Ordering::Acquire) {
            if let Some(rss) = current_rss_bytes() {
                monitor_peak.fetch_max(rss, Ordering::Relaxed);
            }
            thread::sleep(Duration::from_millis(20));
        }
    });
    let paths = (0..path_count)
        .map(|index| {
            format!(
                "src/package-{bucket:04}/module-{index:07}/file-{index:07}.rs",
                bucket = index % 4096
            )
        })
        .collect::<Vec<_>>();
    let average_path_bytes = paths.iter().map(String::len).sum::<usize>() / paths.len().max(1);

    let notifications = Arc::new(AtomicUsize::new(0));
    let notify_count = notifications.clone();
    let (notify_tx, notify_rx) = mpsc::channel();
    let options = SearchSessionOptions::new(SessionGeneration(1), move |generation| {
        notify_count.fetch_add(1, Ordering::Relaxed);
        let _ = notify_tx.send(generation);
    });
    let mut session =
        SearchSession::start_with_paths(&root, options, paths).expect("start benchmark session");

    let session_started = Instant::now();
    session.update(SearchMode::fuzzy("file"));
    let first_snapshot = wait_for_snapshot(&session, &notify_rx, |snapshot| {
        snapshot.mode == SearchMode::fuzzy("file")
    });
    let first_progress = session_started.elapsed();
    let completed = if matches!(first_snapshot.phase, SearchPhase::Complete) {
        first_snapshot
    } else {
        wait_for_snapshot(&session, &notify_rx, |snapshot| {
            snapshot.mode == SearchMode::fuzzy("file")
                && matches!(snapshot.phase, SearchPhase::Complete)
        })
    };
    let build_elapsed = session_started.elapsed();
    let build_notifications = notifications.load(Ordering::Relaxed);
    let completed_rss = current_rss_bytes().unwrap_or(0);

    let append_transitions = [
        ("file", "file-0"),
        ("file-0", "file-00"),
        ("file-00", "file-000"),
        ("file-000", "file-0000"),
        ("file-0000", "file-00000"),
    ];
    let mut append_latencies = Vec::new();
    let mut installed_query = "file";
    for _ in 0..5 {
        for (base, query) in append_transitions {
            if installed_query != base {
                session.update(SearchMode::fuzzy(base));
                let _ = wait_for_snapshot(&session, &notify_rx, |snapshot| {
                    snapshot.mode == SearchMode::fuzzy(base)
                        && matches!(snapshot.phase, SearchPhase::Complete)
                });
            }
            let started = Instant::now();
            session.update(SearchMode::fuzzy(query));
            let _ = wait_for_snapshot(&session, &notify_rx, |snapshot| {
                snapshot.mode == SearchMode::fuzzy(query)
                    && matches!(snapshot.phase, SearchPhase::Complete)
            });
            append_latencies.push((query, started.elapsed()));
            installed_query = query;
        }
    }

    let arbitrary_queries = ["module-1", "pkg", "src/file", "package-40", "file-9"];
    let mut arbitrary_latencies = Vec::new();
    for _ in 0..5 {
        for query in arbitrary_queries {
            let started = Instant::now();
            session.update(SearchMode::fuzzy(query));
            let _ = wait_for_snapshot(&session, &notify_rx, |snapshot| {
                snapshot.mode == SearchMode::fuzzy(query)
                    && matches!(snapshot.phase, SearchPhase::Complete)
            });
            arbitrary_latencies.push((query, started.elapsed()));
        }
    }

    let publication_rate =
        build_notifications.saturating_sub(1) as f64 / build_elapsed.as_secs_f64().max(0.001);
    session.join();
    monitor_stop.store(true, Ordering::Release);
    let _ = monitor.join();
    let peak_increment = peak_rss
        .load(Ordering::Relaxed)
        .saturating_sub(baseline_rss);
    let completed_increment = completed_rss.saturating_sub(baseline_rss);
    let append_p95 = percentile_95(&mut append_latencies);
    let arbitrary_p95 = percentile_95(&mut arbitrary_latencies);

    println!(
        "platform={}-{}",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    println!(
        "cpus={}",
        std::thread::available_parallelism().map_or(1, |n| n.get())
    );
    println!("paths={path_count}");
    println!("average_path_bytes={average_path_bytes}");
    println!(
        "build_mode={}",
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
    );
    println!("matched_paths={}", completed.matches.len());
    println!("build_elapsed_ms={:.2}", millis(build_elapsed));
    println!("first_progress_ms={:.2}", millis(first_progress));
    println!("append_p95_ms={:.2}", millis(append_p95));
    println!("arbitrary_reparse_p95_ms={:.2}", millis(arbitrary_p95));
    for (query, latency) in append_latencies.iter().rev().take(5) {
        println!("slow_append_ms[{query}]={:.2}", millis(*latency));
    }
    println!("completed_incremental_rss_bytes={completed_increment}");
    println!("peak_incremental_rss_bytes={peak_increment}");
    println!("snapshot_publication_rate_hz={publication_rate:.2}");

    if assert_slo {
        assert!(first_progress <= Duration::from_millis(100));
        assert!(append_p95 <= Duration::from_millis(50));
        assert!(arbitrary_p95 <= Duration::from_millis(150));
        assert!(completed_increment <= COMPLETED_RSS_LIMIT);
        assert!(peak_increment <= PEAK_RSS_LIMIT);
        assert!(publication_rate <= 60.0);
    }
}

fn wait_for_snapshot(
    session: &SearchSession,
    notifications: &mpsc::Receiver<SessionGeneration>,
    predicate: impl Fn(&SearchSnapshot) -> bool,
) -> SearchSnapshot {
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        notifications
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("benchmark snapshot notification");
        if let Some(snapshot) = session.take_latest_snapshot()
            && predicate(&snapshot)
        {
            return snapshot;
        }
    }
}

fn percentile_95(values: &mut [(&str, Duration)]) -> Duration {
    values.sort_unstable_by_key(|(_, duration)| *duration);
    let rank = (values.len() * 95).div_ceil(100).saturating_sub(1);
    values[rank].1
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

fn current_rss_bytes() -> Option<usize> {
    let output = Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()?;
    let kilobytes = String::from_utf8(output.stdout)
        .ok()?
        .trim()
        .parse::<usize>()
        .ok()?;
    Some(kilobytes * 1024)
}

fn tempfile_dir() -> std::path::PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("orca-file-search-bench-{}", std::process::id()));
    std::fs::create_dir_all(&path).expect("create benchmark root");
    path
}
