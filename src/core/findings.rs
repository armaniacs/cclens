//! The briefing's findings assembly: pure wiring from fetched rows to
//! `Findings`. The fetch (store queries) and the print stay with the shell
//! (`cli.rs`); this module owns the HashMap joins, the caps, the routing, and
//! the sort — the parts whose bugs showed up as report glitches rather than
//! wrong queries (`pbi/2026-10-29-04-feat-findings-assembly-core.md`).

use std::collections::{BTreeSet, HashMap};

use crate::core::friction::ErrorCategory;
use crate::core::optimize::{
    AgentCost, Findings, FrictionCat, ProjectFindings, SurfaceRef, ThrashLine,
};
use crate::core::scope::split_friction;
use crate::core::surface::{HEAVY_TOKENS, LoadMode, is_usage_measurable, on_demand_tokens};
use crate::core::thrash::{FileEdit, detect_thrash};

/// Top Bash leading words the briefing lists.
pub const TOP_COMMANDS_LIMIT: usize = 15;
/// Most-edited files the briefing lists.
pub const HOTSPOTS_LIMIT: usize = 15;
/// Example excerpts fetched per `(project, category)` — the cap is part of
/// the query, so it lives here as the spec it is.
pub const EXAMPLES_PER_CATEGORY: u32 = 3;
/// Example excerpts pooled per global friction category in the briefing.
pub const EXAMPLES_PER_GLOBAL_CATEGORY: usize = 3;
/// Thrash episodes listed per project: a longer tail is noise for ranking.
pub const THRASH_LINES_PER_PROJECT: usize = 5;
/// Max quiet gap that still joins two edits into one thrash episode.
pub const THRASH_GAP_SECS: i64 = 5 * 60;
/// Edits needed within the gap before a burst counts as thrash at all.
pub const THRASH_MIN_EDITS: u32 = 4;

/// One catalog row as the assembly sees it: plain data, mapped from the
/// store's row type by the fetch (`cli.rs`), so this module stays store-free.
#[derive(Debug, Clone)]
pub struct CatalogRow {
    pub kind: String,
    pub id: String,
    pub scope: String,
    pub project: String,
    pub startup_tokens: Option<i64>,
    pub static_tokens: Option<i64>,
    pub load_mode: String,
    pub uses: i64,
}

/// Everything the briefing's assembly needs, fetched by the shell. Plain rows
/// only — no store types, no queries, so every join below is unit-testable.
#[derive(Debug, Default)]
pub struct FindingsInput {
    /// The empirical always-on floor; `None` when no session start was seen
    /// (the briefing's "unknown" sentinel — never a guessed zero).
    pub floor: Option<i64>,
    /// `(project, category, excerpt)`, already capped per group by the fetch.
    pub error_examples: Vec<(String, String, String)>,
    /// `(project, category, tool, count)` attribution rows.
    pub error_breakdown: Vec<(String, String, String, i64)>,
    /// `(project, category, count)` friction cells, before the actionable
    /// filter — the filter is part of the routing, so it runs here.
    pub friction_cells: Vec<(String, String, i64)>,
    /// Bash leading words, most frequent first.
    pub bash_commands: Vec<(String, i64)>,
    /// Most-edited files (basename counts), most frequent first.
    pub file_edit_counts: Vec<(String, i64)>,
    /// Every file edit, for thrash detection.
    pub file_edits: Vec<FileEdit>,
    /// The installed-config catalog with usage, for the wedge routing.
    pub catalog: Vec<CatalogRow>,
    /// `(root, project)` pairs for the per-project sections.
    pub session_roots: Vec<(String, String)>,
    /// Prompt behavioral-class counts.
    pub prompt_behaviors: Vec<(String, i64)>,
    /// `(subagent output tokens, subagent runs)` over the whole store.
    pub subagent_totals: (i64, i64),
    /// Per-agent-type costs, costliest first as fetched.
    pub agents: Vec<AgentCost>,
    /// Each skill invocation's output tokens — summed here into `main_out`.
    pub skill_out_tokens: Vec<i64>,
    /// Global always-on config tokens as fetched (applied only when the
    /// floor is known — see `collect_findings`).
    pub always_on_config_tokens: i64,
}

/// Wedge-flagged surfaces (unused-but-measurable, always-on heavy) grouped by
/// the config layer that owns them — the routing key for summary and briefing.
#[derive(Default)]
pub struct WedgeRefs {
    pub unused: Vec<SurfaceRef>,
    pub always_on_heavy: Vec<SurfaceRef>,
}

/// The wedges split by the surface's own scope.
#[derive(Default)]
pub struct ScopedWedges {
    pub global: WedgeRefs,
    pub projects: HashMap<String, WedgeRefs>,
}

/// Route the catalog's wedge-flagged surfaces to the config layer that owns
/// each fix: global surfaces to the global bucket, project surfaces to their
/// project's bucket (`docs/specs/surfaces.md`, `cli.md`).
pub fn config_wedges(catalog: &[CatalogRow]) -> ScopedWedges {
    // Carry the cost split, not one figure: a skill's file weight is its
    // invocation cost, and rendering it as the trim saving promised startup
    // context that deleting it never frees (`docs/specs/surfaces.md`).
    let surface_ref = |row: &CatalogRow, load_mode: LoadMode| SurfaceRef {
        kind: row.kind.clone(),
        id: row.id.clone(),
        startup_tokens: row.startup_tokens,
        on_demand_tokens: on_demand_tokens(load_mode, row.static_tokens.map(|t| t as u64))
            .map(|t| t as i64),
    };
    let mut scoped = ScopedWedges::default();
    for row in catalog {
        let refs = if row.scope == "global" {
            &mut scoped.global
        } else {
            scoped.projects.entry(row.project.clone()).or_default()
        };
        let load_mode = LoadMode::from_label(&row.load_mode).unwrap_or(LoadMode::OnDemand);
        let static_tokens = row.static_tokens.map(|t| t as u64);
        if is_usage_measurable(&row.kind) && row.uses == 0 {
            refs.unused.push(surface_ref(row, load_mode));
        }
        if load_mode.is_always_on() && static_tokens.is_some_and(|t| t >= HEAVY_TOKENS) {
            refs.always_on_heavy.push(surface_ref(row, load_mode));
        }
    }
    scoped
}

/// Assemble the complete briefing from fetched rows — every view's detail as
/// owned data for `core::optimize`, so the seeded session works from the
/// briefing and need not re-run the tool. Lists are capped where a long tail
/// adds nothing for ranking.
pub fn collect_findings(input: &FindingsInput) -> Findings {
    // The briefing's "unknown floor" sentinel is 0 — no session start observed
    // means the always-on section is omitted rather than guessed at.
    let floor = input.floor.unwrap_or(0);

    let mut examples: HashMap<(String, String), Vec<String>> = HashMap::new();
    for (project, category, excerpt) in &input.error_examples {
        examples
            .entry((project.clone(), category.clone()))
            .or_default()
            .push(excerpt.clone());
    }

    // The split of each (project, category) across the tools that produced it —
    // so the briefing carries the attribution the agent otherwise re-derives.
    let mut by_tool: HashMap<(String, String), Vec<(String, i64)>> = HashMap::new();
    for (project, category, tool, n) in &input.error_breakdown {
        by_tool
            .entry((project.clone(), category.clone()))
            .or_default()
            .push((tool.clone(), *n));
    }

    // Route actionable friction to the layer that owns each fix (core::scope):
    // majority-owned categories to their project, the spread rest to global.
    let cells: Vec<(String, String, i64)> = input
        .friction_cells
        .iter()
        .filter(|(_, label, _)| ErrorCategory::from_label(label).is_actionable())
        .cloned()
        .collect();
    let split = split_friction(&cells);

    // A global category aggregates its tool split across projects and pools a
    // few examples; a project-owned one uses its own (project, category) detail.
    let friction_global: Vec<FrictionCat> = split
        .global
        .iter()
        .map(|g| {
            let mut tools: HashMap<String, i64> = HashMap::new();
            let mut pooled = Vec::new();
            for (project, label, _) in &cells {
                if label != &g.category {
                    continue;
                }
                let key = (project.clone(), label.clone());
                for (tool, n) in by_tool.get(&key).into_iter().flatten() {
                    *tools.entry(tool.clone()).or_default() += n;
                }
                pooled.extend(examples.get(&key).into_iter().flatten().cloned());
            }
            let mut by_tool: Vec<(String, i64)> = tools.into_iter().collect();
            by_tool.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            pooled.truncate(EXAMPLES_PER_GLOBAL_CATEGORY);
            FrictionCat {
                label: g.category.clone(),
                count: g.total,
                projects: g.projects,
                by_tool,
                examples: pooled,
            }
        })
        .collect();

    // Workflow: Bash mix + cd share, most-edited files (global signals).
    let bash = &input.bash_commands;
    let bash_total: i64 = bash.iter().map(|(_, n)| n).sum();
    let cd_pct = (bash_total > 0).then(|| {
        let cd = bash.iter().find(|(c, _)| c == "cd").map_or(0, |(_, n)| *n);
        (cd as f64 * 100.0 / bash_total as f64).round() as i64
    });
    let top_commands: Vec<(String, i64)> = bash.iter().take(TOP_COMMANDS_LIMIT).cloned().collect();
    let hotspots: Vec<(String, i64)> = input
        .file_edit_counts
        .iter()
        .take(HOTSPOTS_LIMIT)
        .cloned()
        .collect();

    // Thrash per project — a burst belongs to the project it happened in. Each
    // episode already knows its project, so detection runs once over everything.
    let mut thrash_by_project: HashMap<String, Vec<ThrashLine>> = HashMap::new();
    for episode in detect_thrash(&input.file_edits, THRASH_GAP_SECS, THRASH_MIN_EDITS) {
        let lines = thrash_by_project
            .entry(episode.project.clone())
            .or_default();
        if lines.len() < THRASH_LINES_PER_PROJECT {
            lines.push(ThrashLine {
                span_secs: episode.span_secs(),
                file: episode.file().to_string(),
                edits: episode.edits,
            });
        }
    }

    // Config wedges, split by the surface's own scope.
    let scoped = config_wedges(&input.catalog);

    // Assemble per-project findings: any project owning friction, thrash, or
    // project-scoped wedges gets a section, busiest first.
    let roots: HashMap<String, String> = input
        .session_roots
        .iter()
        .map(|(root, project)| (project.clone(), root.clone()))
        .collect();
    let mut owned_friction: HashMap<String, Vec<(String, i64)>> =
        split.per_project.into_iter().collect();
    let project_names: Vec<String> = owned_friction
        .keys()
        .chain(thrash_by_project.keys())
        .chain(scoped.projects.keys())
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut projects: Vec<ProjectFindings> = project_names
        .into_iter()
        .map(|project| {
            let friction = owned_friction
                .remove(&project)
                .unwrap_or_default()
                .into_iter()
                .map(|(label, count)| {
                    let key = (project.clone(), label.clone());
                    FrictionCat {
                        by_tool: by_tool.get(&key).cloned().unwrap_or_default(),
                        examples: examples.get(&key).cloned().unwrap_or_default(),
                        label,
                        count,
                        projects: 1,
                    }
                })
                .collect();
            let wedges = scoped.projects.get(&project);
            ProjectFindings {
                friction,
                thrash: thrash_by_project.remove(&project).unwrap_or_default(),
                unused: wedges.map(|w| w.unused.clone()).unwrap_or_default(),
                always_on_heavy: wedges
                    .map(|w| w.always_on_heavy.clone())
                    .unwrap_or_default(),
                root: roots.get(&project).cloned().unwrap_or_default(),
                project,
            }
        })
        .collect();
    // Busiest first: owned friction, then config wedges, then thrash — so a
    // friction-free but config-heavy project still outranks thrash-only noise.
    projects.sort_by(|a, b| {
        let weight = |p: &ProjectFindings| {
            (
                p.total_friction(),
                p.unused.len() + p.always_on_heavy.len(),
                p.thrash.len(),
            )
        };
        weight(b)
            .cmp(&weight(a))
            .then_with(|| a.project.cmp(&b.project))
    });

    let pcounts = &input.prompt_behaviors;
    let ptotal: i64 = pcounts.iter().map(|(_, n)| n).sum();
    let share = |name: &str| {
        let n = pcounts
            .iter()
            .find(|(b, _)| b == name)
            .map_or(0, |(_, n)| *n);
        (n as f64 * 100.0 / ptotal as f64).round() as i64
    };
    let (steer_pct, correct_pct) = if ptotal > 0 {
        (Some(share("steer")), Some(share("correct")))
    } else {
        (None, None)
    };

    let (sub_tokens, sub_agents) = input.subagent_totals;
    let agents = input.agents.clone();
    let agent_split = crate::core::subagent::SplitCoverage::new(
        sub_tokens,
        sub_agents,
        agents.iter().map(|a| (a.runs, a.out_tokens)),
    );
    Findings {
        main_out: input.skill_out_tokens.iter().sum(),
        sub_tokens,
        sub_agents,
        agent_split,
        agents,
        floor,
        config_tokens: if floor > 0 {
            input.always_on_config_tokens
        } else {
            0
        },
        friction_global,
        cd_pct,
        top_commands,
        hotspots,
        unused: scoped.global.unused,
        always_on_heavy: scoped.global.always_on_heavy,
        steer_pct,
        correct_pct,
        projects,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_input() -> FindingsInput {
        FindingsInput::default()
    }

    #[test]
    fn named_limits_cap_the_briefings_lists() {
        // Every cap the briefing applies is a name a test can reference —
        // the anonymous `take(15)` / `truncate(3)` literals this module
        // replaces (PBI 04).
        let mut input = empty_input();
        input.bash_commands = (0..TOP_COMMANDS_LIMIT + 10)
            .map(|i| (format!("cmd{i}"), (TOP_COMMANDS_LIMIT + 10 - i) as i64))
            .collect();
        input.file_edit_counts = (0..HOTSPOTS_LIMIT + 10)
            .map(|i| (format!("f{i}.rs"), (HOTSPOTS_LIMIT + 10 - i) as i64))
            .collect();
        // The fetch caps per-category examples with EXAMPLES_PER_CATEGORY;
        // the global pool is capped again at assembly.
        input.error_examples = (0..EXAMPLES_PER_GLOBAL_CATEGORY + 7)
            .map(|i| {
                (
                    "alpha".to_string(),
                    "path-not-found".to_string(),
                    format!("missing /{i}"),
                )
            })
            .collect();
        input.friction_cells = vec![
            ("alpha".to_string(), "path-not-found".to_string(), 3),
            ("beta".to_string(), "path-not-found".to_string(), 3),
        ];
        input.error_breakdown = vec![(
            "alpha".to_string(),
            "path-not-found".to_string(),
            "Read".to_string(),
            6,
        )];
        // Seven thrash episodes in one project — only THRASH_LINES_PER_PROJECT
        // lines may survive.
        input.file_edits = (0..THRASH_LINES_PER_PROJECT + 2)
            .flat_map(|file| {
                (0..THRASH_MIN_EDITS as usize).map(move |i| FileEdit {
                    session_id: "s1".to_string(),
                    project: "alpha".to_string(),
                    path: format!("/repo/f{file}.rs"),
                    epoch: (file * 1000 + i * 60) as i64,
                })
            })
            .collect();

        let findings = collect_findings(&input);

        assert_eq!(findings.top_commands.len(), TOP_COMMANDS_LIMIT);
        assert_eq!(findings.hotspots.len(), HOTSPOTS_LIMIT);
        assert_eq!(
            findings.friction_global[0].examples.len(),
            EXAMPLES_PER_GLOBAL_CATEGORY
        );
        let alpha = findings
            .projects
            .iter()
            .find(|p| p.project == "alpha")
            .expect("alpha has thrash");
        assert_eq!(alpha.thrash.len(), THRASH_LINES_PER_PROJECT);
    }

    #[test]
    fn assembly_routes_and_aggregates_like_the_recorded_briefing() {
        let mut input = empty_input();
        input.floor = Some(1000);
        input.always_on_config_tokens = 700;
        input.bash_commands = vec![("ls".to_string(), 15), ("cd".to_string(), 5)];
        input.prompt_behaviors = vec![
            ("instruct".to_string(), 2),
            ("steer".to_string(), 1),
            ("correct".to_string(), 1),
        ];
        input.skill_out_tokens = vec![10, 20];
        input.subagent_totals = (60, 2);
        input.session_roots = vec![("/repo/alpha".to_string(), "alpha".to_string())];
        // `path-not-found` concentrates in alpha (project-owned);
        // `edit-precondition` ties across two projects (global);
        // `cancelled` is not actionable and must be dropped entirely.
        input.friction_cells = vec![
            ("alpha".to_string(), "path-not-found".to_string(), 6),
            ("beta".to_string(), "path-not-found".to_string(), 1),
            ("alpha".to_string(), "edit-precondition".to_string(), 3),
            ("beta".to_string(), "edit-precondition".to_string(), 3),
            ("gamma".to_string(), "cancelled".to_string(), 9),
        ];
        input.error_breakdown = vec![
            (
                "alpha".to_string(),
                "path-not-found".to_string(),
                "Read".to_string(),
                6,
            ),
            (
                "alpha".to_string(),
                "edit-precondition".to_string(),
                "Edit".to_string(),
                2,
            ),
            (
                "beta".to_string(),
                "edit-precondition".to_string(),
                "Write".to_string(),
                3,
            ),
        ];
        input.error_examples = vec![(
            "alpha".to_string(),
            "edit-precondition".to_string(),
            "no such file".to_string(),
        )];

        let findings = collect_findings(&input);

        assert_eq!(findings.floor, 1000);
        assert_eq!(findings.config_tokens, 700);
        assert_eq!(findings.main_out, 30);
        assert_eq!(findings.cd_pct, Some(25));
        assert_eq!(findings.steer_pct, Some(25));
        assert_eq!(findings.correct_pct, Some(25));
        assert_eq!(findings.sub_tokens, 60);
        assert_eq!(findings.sub_agents, 2);

        // Global = the tied, spread category; its tool split pools across
        // projects, densest tool first, and its examples pool from the cells.
        assert_eq!(findings.friction_global.len(), 1);
        let global = &findings.friction_global[0];
        assert_eq!(global.label, "edit-precondition");
        assert_eq!(global.count, 6);
        assert_eq!(global.projects, 2);
        assert_eq!(
            global.by_tool,
            vec![("Write".to_string(), 3), ("Edit".to_string(), 2)]
        );
        assert_eq!(
            global.examples,
            vec!["no such file".to_string()],
            "the pool comes from the fetched examples, not from the count rows"
        );

        // Project-owned friction lands on alpha with its own root; the
        // non-actionable category never reaches any section.
        let alpha = findings
            .projects
            .iter()
            .find(|p| p.project == "alpha")
            .expect("alpha owns friction");
        assert_eq!(alpha.root, "/repo/alpha");
        assert_eq!(alpha.friction.len(), 1);
        assert_eq!(alpha.friction[0].label, "path-not-found");
        assert_eq!(alpha.friction[0].count, 6);
        assert_eq!(alpha.friction[0].by_tool, vec![("Read".to_string(), 6)]);
        assert!(
            !findings.projects.iter().any(|p| p.project == "gamma"),
            "cancelled is not actionable — no section, no routing"
        );
    }

    #[test]
    fn no_observed_floor_reads_as_unknown_not_as_a_cost() {
        // The briefing's "unknown floor" sentinel is 0: no session start seen
        // means the always-on section is omitted rather than guessed at, and
        // the global figure is not claimed either.
        let mut input = empty_input();
        input.floor = None;
        input.always_on_config_tokens = 999;

        let findings = collect_findings(&input);

        assert_eq!(findings.floor, 0);
        assert_eq!(findings.config_tokens, 0);
        assert_eq!(findings.cd_pct, None);
        assert_eq!(findings.steer_pct, None);
    }
}
