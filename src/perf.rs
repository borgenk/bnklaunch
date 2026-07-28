//! The performance gate: a fixed set of scenarios, timed against a committed
//! baseline, run by `make perf`.
//!
//! Wall-clock timing is noisy. Runs of the same binary on the same machine
//! swing by double-digit percentages depending on what else the box is doing, so
//! a number from a single run says almost nothing. Each scenario is therefore
//! run many times per sample and the *best* sample of several is kept: the
//! fastest run is the one least polluted by whatever else was scheduled, and it
//! is far steadier than a mean. The gate then allows a generous margin over the
//! baseline, because the purpose is to catch a change that made something twice
//! as slow, not to litigate ten percent.
//!
//! What matters here is the launcher's own shape. It lives for seconds, so cold
//! start dominates, and after that the only thing a user can feel is the time
//! between a keystroke and the frame that answers it. The scenarios are the four
//! pieces of those two paths that are pure computation: parsing the desktop
//! files found on disk, searching the catalog they produce, decoding the cache
//! that lets a warm start skip both, and drawing one frame.
//!
//! Update the baseline with `make perf-update`, and say in the commit message
//! what moved and why.

use std::time::Instant;

use crate::desktop::{self, DesktopEntry, MAX_ENTRIES};

/// Samples taken per scenario; the best one is kept.
const SAMPLES: usize = 5;

/// How much slower than the baseline a scenario may run before the gate fails.
/// Wall-clock noise alone accounts for a good fraction of this.
const REGRESSION_RATIO: f64 = 1.25;

const BASELINE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/perf/baseline.txt");

/// One measured scenario: nanoseconds for a single iteration of the work.
struct Measurement {
    name: &'static str,
    ns_per_iter: f64,
}

/// Time f, keeping the best of SAMPLES samples of iters iterations each.
fn bench(name: &'static str, iters: u32, mut f: impl FnMut()) -> Measurement {
    // One untimed pass, so the first sample is not paying for cold caches and
    // first-touch page faults that no later one sees.
    f();

    let mut best = f64::MAX;
    for _ in 0..SAMPLES {
        let start = Instant::now();
        for _ in 0..iters {
            f();
        }
        let ns = start.elapsed().as_nanos() as f64 / iters as f64;
        if ns < best {
            best = ns;
        }
    }
    Measurement {
        name,
        ns_per_iter: best,
    }
}

/// A .desktop file of the shape the real ones have: the keys the parser reads,
/// interleaved with the many it has to skip.
fn desktop_file(i: usize) -> String {
    format!(
        "[Desktop Entry]\n\
         Version=1.0\n\
         Type=Application\n\
         Name=Application {i}\n\
         GenericName=Thing\n\
         Comment=Does a thing with files and windows\n\
         Exec=/usr/bin/app{i} --flag --other-flag %U\n\
         Icon=application-{i}\n\
         Terminal=false\n\
         Categories=Utility;Development;\n\
         MimeType=text/plain;text/html;\n\
         Keywords=thing;stuff;\n\
         StartupNotify=true\n\
         \n\
         [Desktop Action new-window]\n\
         Name=New Window\n\
         Exec=/usr/bin/app{i} --new-window\n"
    )
}

/// A full catalog, the size the caps allow.
fn catalog() -> desktop::Catalog {
    let mut out = desktop::Catalog::new();
    for i in 0..MAX_ENTRIES {
        let name = format!("Application {i}");
        let exec = format!("/usr/bin/app{i} --flag %U");
        if let Some(entry) = DesktopEntry::new(&name, &exec) {
            let _ = out.push(entry);
        }
    }
    out
}

fn measure_all() -> Vec<Measurement> {
    let files: Vec<String> = (0..64).map(desktop_file).collect();
    let entries = catalog();

    // The cache bytes for a full catalog, as load() would read them off disk.
    let (encoded, _) = crate::cache::encode(&entries, 1, &[]).expect("encode cache");

    // An offscreen frame the size of the real window with a full result list.
    let mut pixels = crate::shm::PixelBuffer::new(
        crate::ui::WINDOW_WIDTH,
        crate::ui::calculate_height(crate::ui::MAX_RESULTS),
    )
    .expect("offscreen buffer");
    let font = crate::font::Font::from_path(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/test-font.ttf"
    ))
    .expect("fixture font");
    let results = desktop::search(&entries, "app");
    let rows: Vec<&DesktopEntry> = results.iter().copied().collect();
    let state = crate::app::AppState::new(desktop::Catalog::new(), Default::default());
    let caret = crate::ui::Caret {
        offset: 3,
        selection: None,
        visible: true,
    };

    vec![
        // Cold start: every .desktop file on the machine is parsed.
        bench("desktop_parse", 200, || {
            for f in &files {
                std::hint::black_box(DesktopEntry::parse(std::hint::black_box(f)));
            }
        }),
        // Every keystroke: the whole catalog is filtered and ranked.
        bench("search_1024", 2_000, || {
            std::hint::black_box(desktop::search(
                std::hint::black_box(&entries),
                std::hint::black_box("app"),
            ));
        }),
        // Warm start: the cache is decoded instead of the files being parsed.
        bench("cache_decode", 500, || {
            std::hint::black_box(crate::cache::decode(std::hint::black_box(&encoded)));
        }),
        // Every frame: a keystroke, and the caret blink twice a second.
        bench("frame_draw", 200, || {
            crate::ui::draw_ui(
                &mut pixels,
                std::hint::black_box("app"),
                &state,
                &rows,
                &font,
                true,
                &caret,
            );
        }),
    ]
}

fn read_baseline() -> std::collections::BTreeMap<String, f64> {
    let mut out = std::collections::BTreeMap::new();
    let Ok(text) = std::fs::read_to_string(BASELINE) else {
        return out;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((name, ns)) = line.split_once(char::is_whitespace) {
            if let Ok(ns) = ns.trim().parse::<f64>() {
                out.insert(name.to_string(), ns);
            }
        }
    }
    out
}

fn write_baseline(measurements: &[Measurement]) {
    let mut text = String::from(
        "# Best-of-5 nanoseconds per iteration, written by `make perf-update`.\n\
         # Machine-specific: a number from another box means nothing. What the\n\
         # gate checks is the ratio against these, on the box that wrote them.\n",
    );
    for m in measurements {
        text.push_str(&format!("{} {:.0}\n", m.name, m.ns_per_iter));
    }
    if let Some(dir) = std::path::Path::new(BASELINE).parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    std::fs::write(BASELINE, text).expect("write baseline");
}

/// Stage-time the real startup path against this machine's own application
/// directories. Not a gate: the numbers depend on how many applications are
/// installed and what the page cache holds, so they mean nothing on another box
/// and nothing between runs. It exists to answer where a cold start's time
/// actually goes, which is not a question a fixture can answer.
fn scan_bench() {
    println!();
    println!("  startup, against the real application directories");
    println!();

    // Cold-ish: the first walk pays whatever the page cache does not hold.
    let start = Instant::now();
    let fingerprint = crate::desktop::dirs_fingerprint();
    let fingerprint_us = start.elapsed().as_secs_f64() * 1e6;

    let mut entries = crate::desktop::Catalog::new();
    let start = Instant::now();
    crate::desktop::discover_entries(&mut entries);
    let discover_us = start.elapsed().as_secs_f64() * 1e6;
    let found = entries.len();

    // The same walk again, with everything now hot in the page cache. The gap
    // between the two is what the kernel had to fetch from disk.
    let start = Instant::now();
    crate::desktop::discover_entries(&mut entries);
    let discover_warm_us = start.elapsed().as_secs_f64() * 1e6;

    let start = Instant::now();
    let (encoded, _) = crate::cache::encode(&entries, fingerprint, &[]).expect("encode");
    let encode_us = start.elapsed().as_secs_f64() * 1e6;

    let start = Instant::now();
    let decoded = crate::cache::decode(&encoded).expect("decode");
    let decode_us = start.elapsed().as_secs_f64() * 1e6;

    println!("  {:<28} {:>10.0} us", "dirs_fingerprint", fingerprint_us);
    println!(
        "  {:<28} {:>10.0} us   ({found} entries)",
        "discover_entries (cold)", discover_us
    );
    println!(
        "  {:<28} {:>10.0} us",
        "discover_entries (warm)", discover_warm_us
    );
    println!("  {:<28} {:>10.0} us", "cache encode", encode_us);
    println!(
        "  {:<28} {:>10.0} us   ({} entries, {} bytes)",
        "cache decode",
        decode_us,
        decoded.entries.len(),
        encoded.len()
    );
    println!();
    println!(
        "  A warm start decodes the cache instead of scanning: {:.0} us against {:.0} us.",
        decode_us, discover_warm_us
    );

    // The two ways of reading the files a walk found, measured against each
    // other on the same machine at the same moment. Everything else about the
    // scan is identical, so the difference is the reading and nothing else.
    let mut paths: crate::platform::arena::ArrayVec<
        crate::platform::arena::ArrayString<{ crate::platform::fs::PATH_CAP }>,
        { crate::desktop::MAX_ENTRIES },
    > = crate::platform::arena::ArrayVec::new();
    crate::desktop::collect_all_paths(&mut paths);

    // One reader per run, selected by the environment, so a caller can evict the
    // page cache between runs and time a genuinely cold read. Timing both in one
    // process is only honest when the files are already hot, because whichever
    // ran first would have warmed them for the other.
    let only = std::env::var("BNKLAUNCH_SCAN_MODE").unwrap_or_default();
    let samples = if only.is_empty() { SAMPLES } else { 1 };

    let mut ring = crate::platform::uring::Ring::new(crate::desktop::RING_ENTRIES).ok();
    let mut best_ring = f64::MAX;
    let mut best_serial = f64::MAX;
    for _ in 0..samples {
        if only == "serial" {
            let mut out = crate::desktop::Catalog::new();
            let start = Instant::now();
            crate::desktop::read_entries(&paths, &mut out);
            best_serial = start.elapsed().as_secs_f64() * 1e6;
            continue;
        }
        if only == "ring" {
            let Some(ring) = ring.as_mut() else {
                println!("  (no ring available)");
                return;
            };
            let mut out = crate::desktop::Catalog::new();
            let start = Instant::now();
            crate::desktop::read_entries_batched(ring, &paths, &mut out);
            best_ring = start.elapsed().as_secs_f64() * 1e6;
            continue;
        }

        if let Some(ring) = ring.as_mut() {
            let mut out = crate::desktop::Catalog::new();
            let start = Instant::now();
            crate::desktop::read_entries_batched(ring, &paths, &mut out);
            best_ring = best_ring.min(start.elapsed().as_secs_f64() * 1e6);
        }

        let mut out = crate::desktop::Catalog::new();
        let start = Instant::now();
        crate::desktop::read_entries(&paths, &mut out);
        best_serial = best_serial.min(start.elapsed().as_secs_f64() * 1e6);
    }

    // What a ring costs simply to exist, which is all the event loop pays for
    // one: nothing it submits there can block, so no kernel worker pool is ever
    // stood up. This is the setup syscall and the ring mappings, and no more.
    let mut best_setup = f64::MAX;
    for _ in 0..SAMPLES {
        let start = Instant::now();
        let made = crate::platform::uring::Ring::new(32);
        let us = start.elapsed().as_secs_f64() * 1e6;
        if made.is_ok() {
            best_setup = best_setup.min(us);
        }
    }
    if best_setup < f64::MAX {
        println!();
        println!(
            "  {:<28} {:>10.0} us",
            "Ring::new, nothing blocking", best_setup
        );
    }

    println!();
    println!(
        "  reading the {} files a walk found, best of {SAMPLES}",
        paths.len()
    );
    println!("  {:<28} {:>10.0} us", "one at a time", best_serial);
    if ring.is_some() {
        println!(
            "  {:<28} {:>10.0} us",
            "batched through the ring", best_ring
        );
        println!("  {:<28} {:>9.2}x", "ratio", best_ring / best_serial);
    } else {
        println!("  (no ring available on this kernel)");
    }
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Print where a cold start's time goes. Never fails; `make scan-bench`
    /// runs it.
    #[test]
    #[ignore]
    fn startup_stages() {
        scan_bench();
    }

    /// Run every scenario and fail if one has grown past the allowed margin over
    /// the committed baseline. Ignored by default, since it takes seconds and
    /// means nothing on a loaded machine; `make perf` runs it.
    #[test]
    #[ignore]
    fn perf_gate() {
        let measurements = measure_all();
        let baseline = read_baseline();

        if std::env::var("BNKLAUNCH_PERF_UPDATE").is_ok() {
            write_baseline(&measurements);
            println!("\nbaseline written to perf/baseline.txt\n");
            for m in &measurements {
                println!("  {:<16} {:>12.0} ns", m.name, m.ns_per_iter);
            }
            return;
        }

        println!();
        println!(
            "  {:<16} {:>12} {:>12} {:>8}",
            "scenario", "ns/iter", "baseline", "ratio"
        );

        let mut regressed = Vec::new();
        for m in &measurements {
            match baseline.get(m.name) {
                Some(&base) if base > 0.0 => {
                    let ratio = m.ns_per_iter / base;
                    println!(
                        "  {:<16} {:>12.0} {:>12.0} {:>7.2}x{}",
                        m.name,
                        m.ns_per_iter,
                        base,
                        ratio,
                        if ratio > REGRESSION_RATIO {
                            "  <-- "
                        } else {
                            ""
                        }
                    );
                    if ratio > REGRESSION_RATIO {
                        regressed.push(format!(
                            "{}: {:.0} ns vs {:.0} ns baseline ({:.2}x)",
                            m.name, m.ns_per_iter, base, ratio
                        ));
                    }
                }
                _ => println!(
                    "  {:<16} {:>12.0} {:>12} {:>8}",
                    m.name, m.ns_per_iter, "-", "new"
                ),
            }
        }
        println!();

        assert!(
            regressed.is_empty(),
            "slower than the baseline by more than {REGRESSION_RATIO}x:\n  {}\n\n\
             If the change is a deliberate trade, re-baseline with `make perf-update` \
             and say what moved in the commit message.",
            regressed.join("\n  ")
        );
    }
}
