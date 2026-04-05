/// Persona-based benchmark suite for roux.
///
/// Tests retrieval quality across real-world repos representing different developer personas.
/// Requires repos cloned at /tmp/roux-sources/. Run with:
///   cargo test --test bench_personas -- --ignored --nocapture

struct PersonaQuery {
    query: &'static str,
    expected: &'static [&'static str],
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
        PersonaQuery {
            query: "SearcherBuilder",
            expected: &["SearcherBuilder", "Searcher", "build"],
        },
        PersonaQuery {
            query: "how does line buffering work during search",
            expected: &["LineBuffer", "LineIter", "LineStep"],
        },
        PersonaQuery {
            query: "binary file detection",
            expected: &["BinaryDetection", "quit", "convert"],
        },
        PersonaQuery {
            query: "SinkMatch",
            expected: &["SinkMatch", "SinkContext", "SinkFinish"],
        },
        PersonaQuery {
            query: "memory mapped file search",
            expected: &["MmapChoice", "mmap"],
        },
        PersonaQuery {
            query: "how are search results printed",
            expected: &["Summary", "SummaryBuilder", "Stats"],
        },
        PersonaQuery {
            query: "gitignore override glob matching",
            expected: &["Override", "OverrideBuilder", "Gitignore"],
        },
        PersonaQuery {
            query: "color output and hyperlinks in printer",
            expected: &["ColorSpecs", "SummaryBuilder", "hyperlink"],
        },
    ],
};

// ─── Data scientist (pandas) ────────────────────────────────────────

const PERSONA_PANDAS: Persona = Persona {
    name: "data-scientist (pandas)",
    path: "/tmp/roux-sources/pandas",
    language: "python",
    queries: &[
        PersonaQuery {
            query: "DataFrame merge join",
            expected: &["merge", "merge_ordered", "merge_asof"],
        },
        PersonaQuery {
            query: "groupby aggregation",
            expected: &["groupby", "aggregate", "agg", "apply"],
        },
        PersonaQuery {
            query: "pivot_table",
            expected: &["pivot_table", "pivot", "unstack"],
        },
        PersonaQuery {
            query: "how does fillna handle missing data",
            expected: &["fillna", "dropna"],
        },
        PersonaQuery {
            query: "sort_values",
            expected: &["sort_values", "sort_index"],
        },
        PersonaQuery {
            query: "read_csv parsing options",
            expected: &["read_csv", "TextFileReader"],
        },
        PersonaQuery {
            query: "DataFrame to_parquet export",
            expected: &["to_parquet", "ParquetImpl"],
        },
        PersonaQuery {
            query: "value_counts frequency distribution",
            expected: &["value_counts", "describe", "info"],
        },
    ],
};

// ─── JS frontend (remix) ────────────────────────────────────────────

const PERSONA_REMIX: Persona = Persona {
    name: "js-frontend (remix)",
    path: "/tmp/roux-sources/remix",
    language: "typescript",
    queries: &[
        PersonaQuery {
            query: "createRouter",
            expected: &["createRouter", "Router", "RouterOptions"],
        },
        PersonaQuery {
            query: "session storage cookie",
            expected: &["createCookieSessionStorage", "SessionStorage", "Session"],
        },
        PersonaQuery {
            query: "createCookie",
            expected: &["createCookie", "Cookie", "CookieOptions"],
        },
        PersonaQuery {
            query: "middleware request handling",
            expected: &["Middleware", "asyncContext", "getContext"],
        },
        PersonaQuery {
            query: "Route pattern matching",
            expected: &["RoutePattern", "Route", "RouteMap"],
        },
        PersonaQuery {
            query: "controller action request handler",
            expected: &["Controller", "isController", "RequestHandler"],
        },
        PersonaQuery {
            query: "file storage backend",
            expected: &["FileStorage", "createFsSessionStorage"],
        },
        PersonaQuery {
            query: "method override",
            expected: &["methodOverride", "MethodOverrideOptions"],
        },
    ],
};

// ─── Go backend (gin) ───────────────────────────────────────────────

const PERSONA_GIN: Persona = Persona {
    name: "go-backend (gin)",
    path: "/tmp/roux-sources/gin",
    language: "go",
    queries: &[
        PersonaQuery {
            query: "RouterGroup",
            expected: &["RouterGroup", "IRouter", "IRoutes"],
        },
        PersonaQuery {
            query: "JSON response binding",
            expected: &["JSON", "BindJSON", "ShouldBindJSON"],
        },
        PersonaQuery {
            query: "how does recovery middleware handle panics",
            expected: &["Recovery", "CustomRecovery", "RecoveryFunc"],
        },
        PersonaQuery {
            query: "Context query parameters",
            expected: &["Query", "QueryArray", "QueryMap", "Param"],
        },
        PersonaQuery {
            query: "BasicAuth",
            expected: &["BasicAuth", "Accounts"],
        },
        PersonaQuery {
            query: "Engine routing setup",
            expected: &["New", "Default", "Engine", "addRoute"],
        },
        PersonaQuery {
            query: "redirect and cookie handling",
            expected: &["Redirect", "SetCookie"],
        },
        PersonaQuery {
            query: "HTML template rendering",
            expected: &["HTML", "LoadHTMLGlob", "SetHTMLTemplate"],
        },
    ],
};

// ─── Firmware C++ (Marlin) ──────────────────────────────────────────

const PERSONA_MARLIN: Persona = Persona {
    name: "firmware-cpp (Marlin)",
    path: "/tmp/roux-sources/Marlin",
    language: "cpp",
    queries: &[
        PersonaQuery {
            query: "Stepper motor ISR pulse",
            expected: &["Stepper", "isr", "pulse_phase_isr"],
        },
        PersonaQuery {
            query: "Temperature PID heater control",
            expected: &["Temperature", "manage_heater"],
        },
        PersonaQuery {
            query: "Planner motion block buffering",
            expected: &["Planner", "buffer_line", "recalculate"],
        },
        PersonaQuery {
            query: "G28 homing sequence",
            expected: &["G28", "Motion", "Endstops"],
        },
        PersonaQuery {
            query: "bed leveling probe G29",
            expected: &["G29", "run_z_probe"],
        },
        PersonaQuery {
            query: "hotend set temperature M104",
            expected: &["M104", "M109"],
        },
        PersonaQuery {
            query: "heated bed M140 M190",
            expected: &["M140", "M190"],
        },
        PersonaQuery {
            query: "planner trapezoid recalculation",
            expected: &["recalculate", "recalculate_trapezoids"],
        },
    ],
};

// ─── Metrics ────────────────────────────────────────────────────────

fn hit_at_k(results: &[(Vec<String>, &[&str])], k: usize) -> f64 {
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

fn run_persona(persona: &Persona) -> Option<(f64, f64, f64, f64, usize, usize, u128)> {
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

    let mut results: Vec<(Vec<String>, &[&str])> = Vec::new();

    for q in persona.queries {
        let result = store.search(q.query, 10).unwrap();
        let names: Vec<String> = result.nodes.iter().map(|n| n.name.clone()).collect();
        results.push((names, q.expected));
    }

    let h1 = hit_at_k(&results, 1);
    let h5 = hit_at_k(&results, 5);
    let h10 = hit_at_k(&results, 10);
    let mrr_score = mrr(&results);

    eprintln!(
        "  {:<28} {:>5} sym  {:>5} edges  {:>5}ms",
        persona.name, node_count, edge_count, extract_ms,
    );
    eprintln!(
        "  {:<28} Hit@1:{:>5.1}%  Hit@5:{:>5.1}%  Hit@10:{:>5.1}%  MRR:{:.3}",
        "",
        h1 * 100.0,
        h5 * 100.0,
        h10 * 100.0,
        mrr_score,
    );

    for (i, q) in persona.queries.iter().enumerate() {
        let (ref names, _) = results[i];
        let rank = names
            .iter()
            .position(|n| q.expected.iter().any(|exp| n.contains(exp)))
            .map(|r| r + 1);
        let status = if rank.is_some() { "✓" } else { "✗" };
        let rank_str = rank
            .map(|r| format!("@{r}"))
            .unwrap_or_else(|| "miss".to_string());
        eprintln!("    {status} [{rank_str:>5}] {}", q.query);
    }
    eprintln!();

    Some((h1, h5, h10, mrr_score, node_count, edge_count, extract_ms))
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

    let mut total_h1 = 0.0;
    let mut total_mrr = 0.0;
    let mut count = 0;

    for persona in &personas {
        if let Some((h1, _h5, _h10, mrr_score, _nodes, _edges, _ms)) = run_persona(persona) {
            total_h1 += h1;
            total_mrr += mrr_score;
            count += 1;
        }
    }

    if count > 0 {
        eprintln!("── aggregate ({count} personas) ──");
        eprintln!(
            "  Avg Hit@1: {:.1}%  Avg MRR: {:.3}",
            total_h1 / count as f64 * 100.0,
            total_mrr / count as f64,
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
