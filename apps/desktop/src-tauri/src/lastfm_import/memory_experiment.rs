use std::{
    env, fs,
    path::PathBuf,
    time::{Duration, Instant},
};

use std::process::Command;

use super::super::Service;

const FIXTURE_ENV: &str = "RETUNE_LASTFM_MEMORY_FIXTURE";
const RELEASE_CYCLES: usize = 3;

#[tokio::test]
#[ignore = "manual macOS process-memory experiment; set RETUNE_LASTFM_MEMORY_FIXTURE to a copied session JSON"]
async fn real_session_park_reload_memory_experiment() {
    let fixture = env::var_os(FIXTURE_ENV)
        .map(PathBuf::from)
        .expect("RETUNE_LASTFM_MEMORY_FIXTURE must point to a copied session JSON");
    let fixture_bytes = fs::metadata(&fixture)
        .expect("could not inspect the copied Last.fm session")
        .len();
    let app_data = tempfile::tempdir().expect("could not create isolated app data");
    let isolated_session = app_data.path().join("lastfm-import.json");
    fs::copy(&fixture, &isolated_session)
        .expect("could not copy Last.fm session into temp app data");
    assert_eq!(
        fs::metadata(&isolated_session)
            .expect("could not inspect isolated Last.fm session")
            .len(),
        fixture_bytes,
        "isolated session copy has a different size"
    );

    let (baseline_footprint_bytes, baseline_peak_bytes) = process_footprint_bytes();
    println!(
        "memory_experiment stage=baseline cycle=0 fixture_bytes={} footprint_bytes={} peak_bytes={}",
        fixture_bytes, baseline_footprint_bytes, baseline_peak_bytes,
    );

    let load_started = Instant::now();
    let service = Service::new(app_data.path());
    let initial_load_ms = load_started.elapsed().as_millis();
    let initial = service.state().await;
    assert!(initial.phase.is_some(), "the isolated fixture did not load");
    let summary_counts = |view: &super::super::ImportStateView| {
        (
            view.total_scrobbles,
            view.included_scrobbles,
            view.processed_scrobbles,
            view.matched_scrobbles,
            view.remaining,
            view.pending_review,
        )
    };
    let expected_counts = summary_counts(&initial);
    let (footprint_bytes, peak_bytes) = process_footprint_bytes();
    println!(
        "memory_experiment pid={} stage=resident cycle=0 fixture_bytes={} load_ms={} total_scrobbles={} included_scrobbles={} remaining={} footprint_bytes={} peak_bytes={}",
        std::process::id(),
        fixture_bytes,
        initial_load_ms,
        initial.total_scrobbles,
        initial.included_scrobbles,
        initial.remaining,
        footprint_bytes,
        peak_bytes,
    );

    for cycle in 1..=RELEASE_CYCLES {
        let park_started = Instant::now();
        service
            .close_importer_window_and_park()
            .await
            .expect("could not park isolated Last.fm session");
        let parked_at = Instant::now();
        let park_ms = park_started.elapsed().as_millis();
        let parked = service.state().await;
        assert!(parked.phase.is_some(), "parking lost the session summary");
        assert_eq!(
            summary_counts(&parked),
            expected_counts,
            "parked summary counts changed in cycle {cycle}"
        );
        let (immediate_footprint_bytes, immediate_peak_bytes) = process_footprint_bytes();
        let immediate_after_park_ms = parked_at.elapsed().as_millis();
        println!(
            "memory_experiment stage=parked_immediate cycle={} park_ms={} immediate_after_park_ms={} total_scrobbles={} included_scrobbles={} remaining={} footprint_bytes={} peak_bytes={}",
            cycle,
            park_ms,
            immediate_after_park_ms,
            parked.total_scrobbles,
            parked.included_scrobbles,
            parked.remaining,
            immediate_footprint_bytes,
            immediate_peak_bytes,
        );
        tokio::time::sleep_until(
            tokio::time::Instant::from_std(parked_at) + Duration::from_millis(200),
        )
        .await;
        let (settled_footprint_bytes, settled_peak_bytes) = process_footprint_bytes();
        let settled_after_park_ms = parked_at.elapsed().as_millis();
        println!(
            "memory_experiment stage=parked_settled cycle={} settled_after_park_ms={} footprint_bytes={} peak_bytes={}",
            cycle, settled_after_park_ms, settled_footprint_bytes, settled_peak_bytes,
        );

        service.set_importer_window_open(true);
        let reload_started = Instant::now();
        let queue = service
            .queue_page(0, 1)
            .await
            .expect("could not reload the isolated Last.fm queue");
        let reopened = service.state().await;
        assert_eq!(
            summary_counts(&reopened),
            expected_counts,
            "reloaded summary counts changed in cycle {cycle}"
        );
        let reload_ms = reload_started.elapsed().as_millis();
        let (footprint_bytes, peak_bytes) = process_footprint_bytes();
        println!(
            "memory_experiment stage=resident cycle={} reload_ms={} queue_items={} queue_total={} total_scrobbles={} included_scrobbles={} footprint_bytes={} peak_bytes={}",
            cycle,
            reload_ms,
            queue.items.len(),
            queue.total,
            reopened.total_scrobbles,
            reopened.included_scrobbles,
            footprint_bytes,
            peak_bytes,
        );
    }
}

fn process_footprint_bytes() -> (u64, u64) {
    let pid = std::process::id().to_string();
    let output = Command::new("/usr/bin/footprint")
        .args(["-p", pid.as_str(), "-f", "bytes", "--noCategories"])
        .output()
        .expect("could not run /usr/bin/footprint for the current test process");
    assert!(
        output.status.success(),
        "/usr/bin/footprint failed for the current test process"
    );
    let output = String::from_utf8_lossy(&output.stdout);
    (
        footprint_metric(&output, "phys_footprint"),
        footprint_metric(&output, "phys_footprint_peak"),
    )
}

fn footprint_metric(output: &str, metric: &str) -> u64 {
    for line in output.lines() {
        let Some((name, value)) = line.trim().split_once(':') else {
            continue;
        };
        if name == metric {
            return value
                .split_whitespace()
                .next()
                .and_then(|value| value.parse().ok())
                .unwrap_or_else(|| panic!("footprint did not return {metric} bytes"));
        }
    }
    panic!("footprint omitted {metric}");
}
