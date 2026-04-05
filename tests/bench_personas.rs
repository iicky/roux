/// Persona-based benchmark suite for roux.
///
/// Tests retrieval quality across real-world repos representing different developer personas.
/// Queries are split into agent (programmatic, precise) and developer (natural language, fuzzy).
/// Requires repos cloned at /tmp/roux-sources/. Run with:
///   cargo test --test bench_personas -- --ignored --nocapture

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
            query: "middleware request handling",
            expected: &["Middleware", "asyncContext", "getContext"],
            mode: QueryMode::Developer,
        },
        PersonaQuery {
            query: "route pattern matching",
            expected: &["RoutePattern", "Route", "RouteMap"],
            mode: QueryMode::Developer,
        },
        PersonaQuery {
            query: "file storage backend for sessions",
            expected: &["FileStorage", "createFsSessionStorage"],
            mode: QueryMode::Developer,
        },
        PersonaQuery {
            query: "method override HTTP verbs",
            expected: &["methodOverride", "MethodOverrideOptions"],
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
            query: "Temperature manage_heater PID control",
            expected: &["Temperature", "manage_heater"],
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
            query: "bed leveling probe command",
            expected: &["G29", "run_z_probe"],
            mode: QueryMode::Developer,
        },
        PersonaQuery {
            query: "set hotend temperature",
            expected: &["M104", "M109"],
            mode: QueryMode::Developer,
        },
        PersonaQuery {
            query: "heated bed temperature control",
            expected: &["M140", "M190"],
            mode: QueryMode::Developer,
        },
    ],
};

// ─── Metrics ────────────────────────────────────────────────────────

fn hit_at_k(results: &[(Vec<String>, &[&str])], k: usize) -> f64 {
    if results.is_empty() {
        return 0.0;
    }
    let hits = results
        .iter()
        .filter(|(names, expected)| {
            names
                .iter()
                .take(k)
                .any(|name| expected.iter().any(|exp| name.contains(exp)))
        })
        .count();
    hits as f64 / results.len() as f64
}

fn mrr(results: &[(Vec<String>, &[&str])]) -> f64 {
    if results.is_empty() {
        return 0.0;
    }
    let sum: f64 = results
        .iter()
        .map(|(names, expected)| {
            for (i, name) in names.iter().enumerate() {
                if expected.iter().any(|exp| name.contains(exp)) {
                    return 1.0 / (i + 1) as f64;
                }
            }
            0.0
        })
        .sum();
    sum / results.len() as f64
}

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
            .position(|n| q.expected.iter().any(|exp| n.contains(exp)))
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

    for persona in &personas {
        if let Some(r) = run_persona(persona) {
            total_agent_h1 += r.agent_h1;
            total_agent_mrr += r.agent_mrr;
            total_dev_h1 += r.dev_h1;
            total_dev_mrr += r.dev_mrr;
            count += 1;
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
