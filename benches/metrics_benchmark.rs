//! Microbenchmarks measuring telemetry recording, histogram merging, and templating performance.

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use std::sync::atomic::Ordering;
use std::time::Duration;
use wsblast::config::parse_human_duration;
use wsblast::metrics::{AggregatedMetrics, LiveMetrics, WorkerMetrics};

fn bench_histogram_recording(c: &mut Criterion) {
    let live = LiveMetrics::new();
    let mut worker_metrics = WorkerMetrics::new(live);

    c.bench_function("hdr_histogram_record_rtt", |b| {
        let dur = Duration::from_micros(250);
        b.iter(|| {
            worker_metrics.record_message_recv(black_box(128), black_box(Some(dur)));
        });
    });
}

fn bench_live_atomic_counters(c: &mut Criterion) {
    let live = LiveMetrics::new();

    c.bench_function("atomic_counter_increment", |b| {
        b.iter(|| {
            live.messages_received
                .fetch_add(black_box(1), Ordering::Relaxed);
            live.bytes_received
                .fetch_add(black_box(128), Ordering::Relaxed);
        });
    });
}

fn bench_duration_parsing(c: &mut Criterion) {
    c.bench_function("parse_human_duration_ms", |b| {
        b.iter(|| {
            let _ = parse_human_duration(black_box("500ms"));
        });
    });

    c.bench_function("parse_human_duration_secs", |b| {
        b.iter(|| {
            let _ = parse_human_duration(black_box("30s"));
        });
    });
}

fn bench_histogram_merge(c: &mut Criterion) {
    let live = LiveMetrics::new();
    let mut workers = Vec::with_capacity(50);
    for _ in 0..50 {
        let mut w = WorkerMetrics::new(live.clone());
        for i in 1..=500 {
            w.record_handshake(Duration::from_micros(i * 10));
            w.record_message_recv(128, Some(Duration::from_micros(i * 5)));
        }
        workers.push(w);
    }

    c.bench_function("merge_50_worker_histograms", |b| {
        b.iter(|| {
            let _ = AggregatedMetrics::merge(
                black_box(workers.clone()),
                black_box(Duration::from_secs(10)),
                black_box(50),
            );
        });
    });
}

criterion_group!(
    benches,
    bench_histogram_recording,
    bench_live_atomic_counters,
    bench_duration_parsing,
    bench_histogram_merge,
);
criterion_main!(benches);
