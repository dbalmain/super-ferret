//! Common local/daemon JSON fields. Catalog protection and opacity describe
//! checked state; watch coverage and pending age describe separate freshness.
use crate::cli::{Context, Exit};
use crate::engine::{Engine, QuerySession};
use crate::json::Object;

pub(crate) fn resources(o: &mut Object<'_>) {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let value = |key: &str| {
        status
            .lines()
            .find_map(|s| s.strip_prefix(key))
            .and_then(|s| s.split_whitespace().next())
            .and_then(|s| s.parse::<u64>().ok())
    };
    let input = ferret_catalog::InputLimits::default();
    let log = ferret_catalog::CompactionLimits::default();
    o.opt_int("current_rss_kb", value("VmRSS:"))
        .opt_int("peak_rss_kb", value("VmHWM:"))
        .object("writer_input_budget", |o| {
            o.int("records", input.records as u64)
                .int("owned_bytes", input.owned_bytes as u64);
        })
        .object("writer_log_budget", |o| {
            o.int("bytes", log.log_bytes)
                .int("records", log.records)
                .int("dirty_percent", log.dirty_percent)
                .int("dead_percent", log.dead_percent);
        });
}

pub(crate) fn catalog_fields(o: &mut Object<'_>, session: &QuerySession) {
    let catalog = session.catalog();
    let protected = catalog
        .dir_ids()
        .filter(|&id| catalog.retained_at(id).is_some())
        .count();
    let opaque = catalog
        .dir_ids()
        .filter(|&id| catalog.entry_count(id).is_none() && catalog.retained_at(id).is_none())
        .count();
    o.int("protected_scopes", protected as u64)
        .int("opaque_directories", opaque as u64)
        .int("catalog_bytes", catalog.bytes_read())
        .int("planner_bytes", session.name_index().bytes() as u64)
        .int(
            "name_postings_bytes",
            catalog.resident_names().map_or(0, |n| n.bytes()) as u64,
        );
}

pub(crate) fn local(context: &Context, census: bool) -> Exit {
    let engine = match Engine::open(&context.index) {
        Ok(engine) => engine,
        Err(error) => {
            crate::cli::error(&error.to_string());
            return Exit::Error;
        }
    };
    if census && engine.is_none() {
        crate::cli::error(&format!(
            "no index in {}: run `ferret index DIR` first",
            context.index.display()
        ));
        return Exit::Error;
    }
    let mut out = Vec::new();
    let mut o = Object::new(&mut out);
    o.bool("host_running", false)
        .str("current_operation", "idle")
        .null("last_successful_refresh")
        .null("last_complete_backstop")
        .int("watch_installed", 0)
        .int("watch_needed", 0)
        .int("watch_failed", 0)
        .null("watch_uncovered")
        .int("pending_scopes", 0)
        .int("pending_bytes", 0)
        .byte_strings("polling_roots", [])
        .null("oldest_pending_ms")
        .null("backstop_reason")
        .bool("writer_busy", false)
        .int("writer_commands", 0)
        .int("refreshes", 0)
        .null("last_refresh_reason")
        .null("refresh_error")
        .null("writer_input_usage");
    resources(&mut o);
    if let Some(engine) = &engine {
        let session = engine.pin();
        crate::find_json::generation(&mut o, Some(session.generation()));
        catalog_fields(&mut o, &session);
        o.bool(
            "fault_retained",
            session
                .catalog()
                .dir_ids()
                .any(|id| session.catalog().retained_at(id).is_some()),
        );
        o.integers("pinned_internal_epochs", engine.pinned_epochs());
        if census {
            crate::stats::json_fields(&mut o, &session);
        }
    } else {
        o.null("generation")
            .int("protected_scopes", 0)
            .int("opaque_directories", 0)
            .int("catalog_bytes", 0)
            .int("planner_bytes", 0)
            .bool("fault_retained", false)
            .integers("pinned_internal_epochs", []);
    }
    o.end();
    out.push(b'\n');
    crate::cli::print("status", &out)
}
