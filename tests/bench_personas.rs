//! Persona-based benchmark suite for roux.
//!
//! Tests retrieval quality across real-world repos representing different developer personas.
//! Queries are split into agent (programmatic, precise) and developer (natural language, fuzzy).
//! Requires repos cloned at /tmp/roux-sources/. Run with:
//!   cargo test --test bench_personas -- --ignored --nocapture

mod common;

use common::{hit_at_k, mrr};

#[derive(Debug, Clone, Copy, PartialEq)]
enum QueryMode {
    /// Agent queries: precise, references specific types/patterns, structured
    Agent,
    /// Developer queries: natural language, domain terms, fuzzy
    Developer,
}

struct PersonaQuery {
    query: &'static str,
    expected: &'static [&'static str],
    mode: QueryMode,
}

struct Persona {
    name: &'static str,
    path: &'static str,
    language: &'static str,
    queries: &'static [PersonaQuery],
}

// ─── Rust CLI dev (ripgrep) ─────────────────────────────────────────

const PERSONA_RIPGREP: Persona = Persona {
    name: "rust-cli (ripgrep)",
    path: "/tmp/roux-sources/ripgrep",
    language: "rust",
    queries: &[
        // Agent queries — precise symbol/type references
        PersonaQuery {
            query: "SearcherBuilder struct and build method",
            expected: &["SearcherBuilder", "Searcher", "build"],
            mode: QueryMode::Agent,
        },
        PersonaQuery {
            query: "SinkMatch SinkContext callback types",
            expected: &["SinkMatch", "SinkContext", "SinkFinish"],
            mode: QueryMode::Agent,
        },
        PersonaQuery {
            query: "BinaryDetection configuration options",
            expected: &["BinaryDetection", "quit", "convert"],
            mode: QueryMode::Agent,
        },
        PersonaQuery {
            query: "ColorSpecs hyperlink printer configuration",
            expected: &["ColorSpecs", "SummaryBuilder", "hyperlink"],
            mode: QueryMode::Agent,
        },
        // Developer queries — natural language, domain-level
        PersonaQuery {
            query: "how does line buffering work during search",
            expected: &["LineBuffer", "LineIter", "LineStep"],
            mode: QueryMode::Developer,
        },
        PersonaQuery {
            query: "memory mapped file search",
            expected: &["MmapChoice", "mmap"],
            mode: QueryMode::Developer,
        },
        PersonaQuery {
            query: "how are search results printed",
            expected: &["Summary", "SummaryBuilder", "Stats"],
            mode: QueryMode::Developer,
        },
        PersonaQuery {
            query: "gitignore override glob matching",
            expected: &["Override", "OverrideBuilder", "Gitignore"],
            mode: QueryMode::Developer,
        },
    ],
};

// ─── Data scientist (pandas) ────────────────────────────────────────

const PERSONA_PANDAS: Persona = Persona {
    name: "data-scientist (pandas)",
    path: "/tmp/roux-sources/pandas",
    language: "python",
    queries: &[
        // Agent queries
        PersonaQuery {
            query: "merge function for DataFrame join operations",
            expected: &["merge", "merge_ordered", "merge_asof"],
            mode: QueryMode::Agent,
        },
        PersonaQuery {
            query: "pivot_table method on DataFrame",
            expected: &["pivot_table", "pivot", "unstack"],
            mode: QueryMode::Agent,
        },
        PersonaQuery {
            query: "read_csv TextFileReader parser",
            expected: &["read_csv", "TextFileReader"],
            mode: QueryMode::Agent,
        },
        PersonaQuery {
            query: "sort_values sort_index DataFrame ordering",
            expected: &["sort_values", "sort_index"],
            mode: QueryMode::Agent,
        },
        // Developer queries
        PersonaQuery {
            query: "groupby aggregation apply",
            expected: &["groupby", "aggregate", "agg", "apply"],
            mode: QueryMode::Developer,
        },
        PersonaQuery {
            query: "how does fillna handle missing data",
            expected: &["fillna", "dropna"],
            mode: QueryMode::Developer,
        },
        PersonaQuery {
            query: "export dataframe to parquet file",
            expected: &["to_parquet", "ParquetImpl"],
            mode: QueryMode::Developer,
        },
        PersonaQuery {
            query: "value counts and frequency distribution",
            expected: &["value_counts", "describe", "info"],
            mode: QueryMode::Developer,
        },
    ],
};

// ─── JS frontend (remix) ────────────────────────────────────────────

const PERSONA_REMIX: Persona = Persona {
    name: "js-frontend (remix)",
    path: "/tmp/roux-sources/remix",
    language: "typescript",
    queries: &[
        // Agent queries
        PersonaQuery {
            query: "createRouter function and RouterOptions",
            expected: &["createRouter", "Router", "RouterOptions"],
            mode: QueryMode::Agent,
        },
        PersonaQuery {
            query: "createCookieSessionStorage SessionStorage",
            expected: &["createCookieSessionStorage", "SessionStorage", "Session"],
            mode: QueryMode::Agent,
        },
        PersonaQuery {
            query: "createCookie Cookie CookieOptions",
            expected: &["createCookie", "Cookie", "CookieOptions"],
            mode: QueryMode::Agent,
        },
        PersonaQuery {
            query: "Controller isController RequestHandler",
            expected: &["Controller", "isController", "RequestHandler"],
            mode: QueryMode::Agent,
        },
        // Developer queries
        PersonaQuery {
            // `Middleware` / `asyncContext` were removed in Remix v2; the
            // current handler-chain entry points are `getContext` and the
            // resource/document request dispatchers.
            query: "middleware request handling",
            expected: &["getContext", "handleResourceRequest", "RequestHandler"],
            mode: QueryMode::Developer,
        },
        PersonaQuery {
            query: "route pattern matching",
            expected: &["RoutePattern", "Route", "RouteMap"],
            mode: QueryMode::Developer,
        },
        PersonaQuery {
            // The actual session storage factory is `createFileSessionStorage`
            // (formerly `createFsSessionStorage` — renamed upstream).
            query: "file storage backend for sessions",
            expected: &["createFileSessionStorage", "FileSessionStorage"],
            mode: QueryMode::Developer,
        },
        PersonaQuery {
            // `methodOverride` was removed in v2; v2 routes form HTTP verbs
            // via the `Form` component / `_method` form field. The doc
            // heading "HTML Form HTTP Verbs" lives at docs/route/action.md.
            query: "form HTTP method routing",
            expected: &["Form", "FormMethod", "useSubmit", "useFetcher"],
            mode: QueryMode::Developer,
        },
    ],
};

// ─── Go backend (gin) ───────────────────────────────────────────────

const PERSONA_GIN: Persona = Persona {
    name: "go-backend (gin)",
    path: "/tmp/roux-sources/gin",
    language: "go",
    queries: &[
        // Agent queries
        PersonaQuery {
            query: "RouterGroup IRouter IRoutes interface",
            expected: &["RouterGroup", "IRouter", "IRoutes"],
            mode: QueryMode::Agent,
        },
        PersonaQuery {
            query: "BindJSON ShouldBindJSON JSON response",
            expected: &["JSON", "BindJSON", "ShouldBindJSON"],
            mode: QueryMode::Agent,
        },
        PersonaQuery {
            query: "BasicAuth Accounts middleware",
            expected: &["BasicAuth", "Accounts"],
            mode: QueryMode::Agent,
        },
        PersonaQuery {
            query: "Recovery CustomRecovery RecoveryFunc panic handler",
            expected: &["Recovery", "CustomRecovery", "RecoveryFunc"],
            mode: QueryMode::Agent,
        },
        // Developer queries
        PersonaQuery {
            query: "context query parameters and URL params",
            expected: &["Query", "QueryArray", "QueryMap", "Param"],
            mode: QueryMode::Developer,
        },
        PersonaQuery {
            query: "engine routing setup and initialization",
            expected: &["New", "Default", "Engine", "addRoute"],
            mode: QueryMode::Developer,
        },
        PersonaQuery {
            query: "redirect and cookie handling",
            expected: &["Redirect", "SetCookie"],
            mode: QueryMode::Developer,
        },
        PersonaQuery {
            query: "HTML template rendering",
            expected: &["HTML", "LoadHTMLGlob", "SetHTMLTemplate"],
            mode: QueryMode::Developer,
        },
    ],
};

// ─── Firmware C++ (Marlin) ──────────────────────────────────────────

const PERSONA_MARLIN: Persona = Persona {
    name: "firmware-cpp (Marlin)",
    path: "/tmp/roux-sources/Marlin",
    language: "cpp",
    queries: &[
        // Agent queries
        PersonaQuery {
            query: "Stepper class isr pulse_phase_isr",
            expected: &["Stepper", "isr", "pulse_phase_isr"],
            mode: QueryMode::Agent,
        },
        PersonaQuery {
            query: "Planner buffer_line recalculate block",
            expected: &["Planner", "buffer_line", "recalculate"],
            mode: QueryMode::Agent,
        },
        PersonaQuery {
            // Marlin uses `manage_hotends` for the heater management loop;
            // there is no `manage_heater` symbol.
            query: "Temperature manage_heater PID control",
            expected: &["Temperature", "manage_hotends", "PID_autotune"],
            mode: QueryMode::Agent,
        },
        PersonaQuery {
            query: "recalculate_trapezoids check_axes_activity",
            expected: &["recalculate", "recalculate_trapezoids"],
            mode: QueryMode::Agent,
        },
        // Developer queries
        PersonaQuery {
            query: "G28 homing sequence",
            expected: &["G28", "Motion", "Endstops"],
            mode: QueryMode::Developer,
        },
        PersonaQuery {
            // G-code symbols appear in both upper- and lowercase forms
            // (e.g. `G29` the handler, `g29_what_command` the helper). Token
            // matching is case-insensitive so either shape hits, but list both
            // for clarity.
            query: "bed leveling probe command",
            expected: &["G29", "g29_", "run_z_probe", "probe_index"],
            mode: QueryMode::Developer,
        },
        PersonaQuery {
            query: "set hotend temperature",
            expected: &["M104", "M109"],
            mode: QueryMode::Developer,
        },
        PersonaQuery {
            // M140/M190 are G-code parsers; `manage_heated_bed` and
            // `setTargetBed` are the actual control logic. Either is a
            // useful answer for an agent asking about heated-bed control.
            query: "heated bed temperature control",
            expected: &["M140", "M190", "manage_heated_bed", "setTargetBed"],
            mode: QueryMode::Developer,
        },
    ],
};

// ─── Runner ─────────────────────────────────────────────────────────

struct PersonaResult {
    h1: f64,
    h5: f64,
    h10: f64,
    mrr: f64,
    agent_h1: f64,
    agent_mrr: f64,
    dev_h1: f64,
    dev_mrr: f64,
    node_count: usize,
    edge_count: usize,
    extract_ms: u128,
}

fn run_persona(persona: &Persona) -> Option<PersonaResult> {
    use roux_cli::graph::extract;
    use roux_cli::graph::store::GraphStore;
    use std::time::Instant;

    let path = std::path::Path::new(persona.path);
    if !path.exists() {
        eprintln!("  SKIP {} (not found at {})", persona.name, persona.path);
        return None;
    }

    let store = GraphStore::open_in_memory().unwrap();

    let t0 = Instant::now();
    let graph = extract::extract_dir(path, persona.name, "dev", Some(persona.language)).unwrap();
    let extract_ms = t0.elapsed().as_millis();

    let node_count = graph.nodes.len();
    let edge_count = graph.edges.len();

    store
        .upsert_source(
            persona.name,
            "dev",
            persona.language,
            &graph.nodes,
            &graph.edges,
        )
        .unwrap();

    let mut all_results: Vec<(Vec<String>, &[&str])> = Vec::new();
    let mut agent_results: Vec<(Vec<String>, &[&str])> = Vec::new();
    let mut dev_results: Vec<(Vec<String>, &[&str])> = Vec::new();

    for q in persona.queries {
        let result = store.search(q.query, 10).unwrap();
        let names: Vec<String> = result.nodes.iter().map(|n| n.name.clone()).collect();

        let rank = names
            .iter()
            .position(|n| common::any_match(n, q.expected))
            .map(|r| r + 1);
        let status = if rank.is_some() { "✓" } else { "✗" };
        let rank_str = rank
            .map(|r| format!("@{r}"))
            .unwrap_or_else(|| "miss".to_string());
        let mode_tag = match q.mode {
            QueryMode::Agent => "agent",
            QueryMode::Developer => "dev  ",
        };
        eprintln!("    {status} [{rank_str:>5}] ({mode_tag}) {}", q.query);

        let entry = (names, q.expected);
        match q.mode {
            QueryMode::Agent => agent_results.push(entry.clone()),
            QueryMode::Developer => dev_results.push(entry.clone()),
        }
        all_results.push(entry);
    }

    let h1 = hit_at_k(&all_results, 1);
    let h5 = hit_at_k(&all_results, 5);
    let h10 = hit_at_k(&all_results, 10);
    let mrr_score = mrr(&all_results);
    let agent_h1 = hit_at_k(&agent_results, 1);
    let agent_mrr = mrr(&agent_results);
    let dev_h1 = hit_at_k(&dev_results, 1);
    let dev_mrr = mrr(&dev_results);

    eprintln!(
        "  {:<28} {:>5} sym  {:>5} edges  {:>5}ms",
        persona.name, node_count, edge_count, extract_ms,
    );
    eprintln!(
        "  {:<28} ALL    Hit@1:{:>5.1}%  Hit@10:{:>5.1}%  MRR:{:.3}",
        "",
        h1 * 100.0,
        h10 * 100.0,
        mrr_score,
    );
    eprintln!(
        "  {:<28} AGENT  Hit@1:{:>5.1}%  MRR:{:.3}  ({} queries)",
        "",
        agent_h1 * 100.0,
        agent_mrr,
        agent_results.len(),
    );
    eprintln!(
        "  {:<28} DEV    Hit@1:{:>5.1}%  MRR:{:.3}  ({} queries)",
        "",
        dev_h1 * 100.0,
        dev_mrr,
        dev_results.len(),
    );
    eprintln!();

    Some(PersonaResult {
        h1,
        h5,
        h10,
        mrr: mrr_score,
        agent_h1,
        agent_mrr,
        dev_h1,
        dev_mrr,
        node_count,
        edge_count,
        extract_ms,
    })
}

// ─── Tests ──────────────────────────────────────────────────────────

#[test]
#[ignore]
fn bench_all_personas() {
    eprintln!("\n═══ Persona Benchmarks ═══\n");

    let personas = [
        &PERSONA_RIPGREP,
        &PERSONA_PANDAS,
        &PERSONA_REMIX,
        &PERSONA_GIN,
        &PERSONA_MARLIN,
    ];

    let mut total_agent_h1 = 0.0;
    let mut total_agent_mrr = 0.0;
    let mut total_dev_h1 = 0.0;
    let mut total_dev_mrr = 0.0;
    let mut count = 0;
    let mut persona_json = Vec::new();

    for persona in &personas {
        if let Some(r) = run_persona(persona) {
            total_agent_h1 += r.agent_h1;
            total_agent_mrr += r.agent_mrr;
            total_dev_h1 += r.dev_h1;
            total_dev_mrr += r.dev_mrr;
            count += 1;
            persona_json.push(serde_json::json!({
                "name": persona.name,
                "language": persona.language,
                "node_count": r.node_count,
                "edge_count": r.edge_count,
                "extract_ms": r.extract_ms,
                "metrics": {
                    "h1": r.h1,
                    "h5": r.h5,
                    "h10": r.h10,
                    "mrr": r.mrr,
                    "agent_h1": r.agent_h1,
                    "agent_mrr": r.agent_mrr,
                    "dev_h1": r.dev_h1,
                    "dev_mrr": r.dev_mrr,
                },
            }));
        }
    }

    if count > 0 {
        eprintln!("── aggregate ({count} personas) ──");
        eprintln!(
            "  AGENT  Avg Hit@1: {:>5.1}%  Avg MRR: {:.3}",
            total_agent_h1 / count as f64 * 100.0,
            total_agent_mrr / count as f64,
        );
        eprintln!(
            "  DEV    Avg Hit@1: {:>5.1}%  Avg MRR: {:.3}",
            total_dev_h1 / count as f64 * 100.0,
            total_dev_mrr / count as f64,
        );
    }

    // Optional JSON emission for CI. Triggered by env var so local runs stay clean.
    if let Ok(path) = std::env::var("ROUX_BENCH_JSON_OUT") {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let commit = std::env::var("GITHUB_SHA").ok();
        let payload = serde_json::json!({
            "roux_version": env!("CARGO_PKG_VERSION"),
            "timestamp": timestamp,
            "commit": commit,
            "personas": persona_json,
            "aggregate": if count > 0 {
                serde_json::json!({
                    "count": count,
                    "agent_h1": total_agent_h1 / count as f64,
                    "agent_mrr": total_agent_mrr / count as f64,
                    "dev_h1": total_dev_h1 / count as f64,
                    "dev_mrr": total_dev_mrr / count as f64,
                })
            } else {
                serde_json::json!(null)
            },
        });
        std::fs::write(&path, serde_json::to_string_pretty(&payload).unwrap())
            .unwrap_or_else(|e| panic!("writing {path}: {e}"));
        eprintln!("\nWrote JSON metrics to {path}");
    }
}

#[test]
#[ignore]
fn bench_persona_ripgrep() {
    run_persona(&PERSONA_RIPGREP);
}

#[test]
#[ignore]
fn bench_persona_pandas() {
    run_persona(&PERSONA_PANDAS);
}

#[test]
#[ignore]
fn bench_persona_remix() {
    run_persona(&PERSONA_REMIX);
}

#[test]
#[ignore]
fn bench_persona_gin() {
    run_persona(&PERSONA_GIN);
}

#[test]
#[ignore]
fn bench_persona_marlin() {
    run_persona(&PERSONA_MARLIN);
}
