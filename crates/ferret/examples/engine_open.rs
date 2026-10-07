//! Measure a fresh process's real query-only engine, without CLI query logging.
//! Usage: engine_open CATALOG QUERY. OS-cache state is the caller's
//! responsibility.

use std::error::Error;
use std::hint::black_box;
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::time::{Instant, SystemTime};

use ferret::engine::Engine;
use ferret_query::Query;

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let [index, text] = args.as_slice() else {
        return Err("usage: engine_open CATALOG QUERY".into());
    };
    let query = Query::parse(
        text.to_str().ok_or("query must be UTF-8")?,
        SystemTime::now(),
    )?;
    let started = Instant::now();
    let engine = Engine::open(&PathBuf::from(index))?.ok_or("no checkpoint")?;
    let open = started.elapsed();
    let pin = engine.pin();
    let (resident, peak) = memory()?;
    let mut first = None;
    let query_started = Instant::now();
    let stats = pin.search(&query, |row| {
        first.get_or_insert_with(|| query_started.elapsed());
        black_box(row.path);
        ControlFlow::Continue(())
    })?;
    let query_time = query_started.elapsed();
    let (after, query_peak) = memory()?;
    println!(
        "names={} open_ms={:.3} rss_kib={} peak_kib={} bytes_per_name={:.3} first_row_ms={} first_row_including_open_ms={} query_ms={:.3} rows={} after_rss_kib={} after_peak_kib={} bytes_read={}",
        pin.catalog().name_count(),
        open.as_secs_f64() * 1000.0,
        resident,
        peak,
        resident as f64 * 1024.0 / f64::from(pin.catalog().name_count()),
        first.map_or_else(
            || "none".into(),
            |time| format!("{:.3}", time.as_secs_f64() * 1000.0)
        ),
        first.map_or_else(
            || "none".into(),
            |time| format!("{:.3}", (open + time).as_secs_f64() * 1000.0)
        ),
        query_time.as_secs_f64() * 1000.0,
        stats.rows,
        after,
        query_peak,
        pin.catalog().bytes_read(),
    );
    Ok(())
}

fn memory() -> Result<(u64, u64), Box<dyn Error>> {
    let status = std::fs::read_to_string("/proc/self/status")?;
    let field = |name: &str| -> Result<u64, Box<dyn Error>> {
        let value = status
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .and_then(|value| value.split_whitespace().next())
            .ok_or("missing RSS field")?;
        Ok(value.parse()?)
    };
    Ok((field("VmRSS:")?, field("VmHWM:")?))
}
