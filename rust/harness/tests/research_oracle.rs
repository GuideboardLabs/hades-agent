//! End-to-end proof for the research core: a fixture web, a two-step plan, two stateless
//! extractors, one stored source each, and a deterministic provenance check.

use std::collections::BTreeMap;
use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};

use harness::research::{
    BenchQuestion, ClaimDraft, ClaimKind, Engine, Extractor, Fetched, PlanItem, Planner,
    ResearchBudget, RunMode, RunStatus, SearchHit, Source, Store, Web, check, run_bench,
};
use harness::scratch;

struct FixtureWeb {
    pages: BTreeMap<String, String>,
}

impl FixtureWeb {
    fn new() -> FixtureWeb {
        let alpha = format!(
            "Alpha report. {} This alpha sentence is copied exactly by the worker. A contradicting observation says the opposite result. {}",
            "The first source explains the mechanism in enough detail to be useful. ".repeat(4),
            "A second sentence gives the reader the context that makes the claim checkable. "
                .repeat(3)
        );
        let beta = format!(
            "Beta report. {} This beta sentence is copied exactly by the worker. {}",
            "The second source adds a separate observation with its own evidence. ".repeat(4),
            "Another sentence keeps the fixture body above the minimum size. ".repeat(4)
        );
        let mut pages = BTreeMap::new();
        pages.insert("https://example.com/alpha".to_string(), alpha);
        pages.insert("https://example.com/beta".to_string(), beta);
        FixtureWeb { pages }
    }
}

impl Web for FixtureWeb {
    fn search(&self, query: &str, _limit: usize) -> Result<Vec<SearchHit>, String> {
        if query.contains("alpha") {
            Ok(vec![SearchHit {
                title: "Alpha report".to_string(),
                url: "https://example.com/alpha".to_string(),
                snippet: "a snippet is not evidence".to_string(),
            }])
        } else {
            Ok(vec![SearchHit {
                title: "Beta report".to_string(),
                url: "https://example.com/beta".to_string(),
                snippet: "a snippet is not evidence".to_string(),
            }])
        }
    }

    fn fetch(&self, url: &str) -> Result<Fetched, String> {
        let body = self
            .pages
            .get(url)
            .ok_or_else(|| format!("the fixture has no page at {url}"))?;
        Ok(Fetched {
            url: url.to_string(),
            final_url: url.to_string(),
            title: "fixture".to_string(),
            status: Some(200),
            content_type: "text/html".to_string(),
            body: body.to_string(),
            excerpt: None,
        })
    }
}

struct FixedPlanner;

impl Planner for FixedPlanner {
    fn plan(&self, _question: &str, _budget: &ResearchBudget) -> Result<Vec<PlanItem>, String> {
        Ok(vec![
            PlanItem {
                text: "What does the alpha source say?".to_string(),
                query: "alpha".to_string(),
                kind: harness::research::QuestionKind::Fact,
            },
            PlanItem {
                text: "What does the beta source say?".to_string(),
                query: "beta".to_string(),
                kind: harness::research::QuestionKind::Fact,
            },
        ])
    }
}

struct FixedExtractor;

impl Extractor for FixedExtractor {
    fn extract(
        &self,
        _subquestion: &str,
        _source: &Source,
        body: &str,
        _budget: &ResearchBudget,
    ) -> Result<Vec<ClaimDraft>, String> {
        let quote = if body.contains("This alpha sentence") {
            "This alpha sentence is copied exactly by the worker."
        } else {
            "This beta sentence is copied exactly by the worker."
        };
        Ok(vec![ClaimDraft {
            text: format!("The source says: {quote}"),
            quote: quote.to_string(),
            kind: ClaimKind::Sourced,
            decision: None,
            alternatives: Vec::new(),
            contradicts: Vec::new(),
        }])
    }
}

fn run() -> (Store, harness::research::ResearchRun) {
    let store = Store::new(&scratch::unique("research-e2e"));
    let web = FixtureWeb::new();
    let planner = FixedPlanner;
    let extractor = FixedExtractor;
    let engine = Engine {
        store: &store,
        web: &web,
        planner: &planner,
        extractor: &extractor,
        mode: RunMode::Web,
    };
    let run = engine
        .run_named(
            "r-test",
            "What happened in the two fixture reports?",
            ResearchBudget {
                queries: 4,
                fetches: 4,
                minutes: 5,
                claims_per_source: 2,
            },
        )
        .expect("a completed run");
    (store, run)
}

#[test]
fn a_research_run_publishes_two_verified_claims() {
    let (store, run) = run();
    assert_eq!(run.status, RunStatus::Published);
    assert_eq!(run.claims.len(), 2);
    assert_eq!(run.sources.len(), 2);
    assert_eq!(run.assignments.len(), 3, "one plan and two workers");
    assert!(run.usage.searches >= 2);
    assert!(run.usage.fetches >= 2);

    let result = check(&store, &run.id).expect("a check");
    assert_eq!(result.code, harness::research::CheckCode::Green);
    assert_eq!(result.verified, 2);

    let report = fs::read_to_string(store.dir(&run.id).join("report.md")).expect("a report");
    assert!(report.contains("[c1]"));
    assert!(report.contains("[c2]"));
    assert!(report.contains("This alpha sentence"));
}

#[test]
fn a_tampered_source_turns_the_check_red() {
    let (store, run) = run();
    let path = store.source_path(&run.id, "s1");
    let body = fs::read_to_string(&path).expect("the source");
    fs::write(&path, body.replace("alpha sentence", "altered sentence")).expect("the tamper");
    let result = check(&store, &run.id).expect("a check");
    assert_eq!(result.code, harness::research::CheckCode::Red);
    assert!(
        result
            .failures
            .iter()
            .any(|line| line.contains("does not match"))
    );
}

#[test]
fn a_missing_source_turns_the_check_unreadable() {
    let (store, run) = run();
    fs::remove_file(store.source_path(&run.id, "s1")).expect("the removal");
    let result = check(&store, &run.id).expect("a check");
    assert_eq!(result.code, harness::research::CheckCode::Unreadable);
}

#[test]
fn a_script_body_is_not_prose() {
    let page = "<html><head><style>.x{color:red}</style><script>var ua = navigator.userAgent;</script></head><body><p>Visible sentence.</p></body></html>";
    let text = harness::agent::strip_html(page);
    assert!(text.contains("Visible sentence."));
    assert!(!text.contains("navigator"), "{text}");
    assert!(!text.contains("color:red"), "{text}");
}

#[test]
fn a_run_with_no_verified_claim_is_not_published() {
    struct Silent;
    impl Extractor for Silent {
        fn extract(
            &self,
            _subquestion: &str,
            _source: &Source,
            _body: &str,
            _budget: &ResearchBudget,
        ) -> Result<Vec<ClaimDraft>, String> {
            Ok(Vec::new())
        }
    }
    let store = Store::new(&scratch::unique("research-empty"));
    let web = FixtureWeb::new();
    let planner = FixedPlanner;
    let extractor = Silent;
    let engine = Engine {
        store: &store,
        web: &web,
        planner: &planner,
        extractor: &extractor,
        mode: RunMode::Web,
    };
    let error = engine
        .run_named(
            "r-empty",
            "What happened?",
            ResearchBudget {
                queries: 4,
                fetches: 4,
                minutes: 5,
                claims_per_source: 2,
            },
        )
        .expect_err("a run without evidence must fail");
    assert!(error.contains("no verified claim"), "{error}");
    let run = store
        .load("r-empty")
        .expect("the failed run is still on disk");
    assert_eq!(run.status, RunStatus::Failed);
    assert!(
        run.failures
            .iter()
            .any(|failure| failure.stage == "progress")
    );
}

struct OneStepPlanner;

impl Planner for OneStepPlanner {
    fn plan(&self, _question: &str, _budget: &ResearchBudget) -> Result<Vec<PlanItem>, String> {
        Ok(vec![PlanItem {
            text: "What does the alpha source say?".to_string(),
            query: "alpha".to_string(),
            kind: harness::research::QuestionKind::Fact,
        }])
    }
}

struct RetryExtractor {
    calls: AtomicUsize,
}

impl Extractor for RetryExtractor {
    fn extract(
        &self,
        _subquestion: &str,
        _source: &Source,
        _body: &str,
        _budget: &ResearchBudget,
    ) -> Result<Vec<ClaimDraft>, String> {
        self.extract_with_hint(_subquestion, _source, _body, _budget, None)
            .map(|(drafts, _)| drafts)
    }

    fn extract_with_hint(
        &self,
        _subquestion: &str,
        _source: &Source,
        _body: &str,
        _budget: &ResearchBudget,
        hint: Option<&str>,
    ) -> Result<(Vec<ClaimDraft>, Option<harness::task::Usage>), String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let quote = if hint.is_some() {
            "This alpha sentence is copied exactly by the worker."
        } else {
            "This quote is not in the fixture at all."
        };
        Ok((
            vec![ClaimDraft {
                text: "The alpha report makes one checkable statement.".to_string(),
                quote: quote.to_string(),
                kind: ClaimKind::Sourced,
                decision: None,
                alternatives: Vec::new(),
                contradicts: Vec::new(),
            }],
            None,
        ))
    }
}

#[test]
fn one_bad_quote_is_retried_once_with_the_mismatch_as_a_hint() {
    let store = Store::new(&scratch::unique("research-retry"));
    let web = FixtureWeb::new();
    let planner = OneStepPlanner;
    let extractor = RetryExtractor {
        calls: AtomicUsize::new(0),
    };
    let engine = Engine {
        store: &store,
        web: &web,
        planner: &planner,
        extractor: &extractor,
        mode: RunMode::Web,
    };
    let run = engine
        .run_named(
            "r-retry",
            "What happened in the alpha fixture?",
            ResearchBudget {
                queries: 2,
                fetches: 2,
                minutes: 5,
                claims_per_source: 2,
            },
        )
        .expect("a completed run");
    assert_eq!(run.status, RunStatus::Published, "{:?}", run.failures);
    assert_eq!(extractor.calls.load(Ordering::SeqCst), 2);
    assert_eq!(run.claims.len(), 1);
}

struct ContradictionExtractor;

impl Extractor for ContradictionExtractor {
    fn extract(
        &self,
        _subquestion: &str,
        _source: &Source,
        _body: &str,
        _budget: &ResearchBudget,
    ) -> Result<Vec<ClaimDraft>, String> {
        Ok(vec![ClaimDraft {
            text: "The alpha report reports a limiting result.".to_string(),
            quote: "This alpha sentence is copied exactly by the worker.".to_string(),
            kind: ClaimKind::Sourced,
            decision: None,
            alternatives: Vec::new(),
            contradicts: vec!["A contradicting observation says the opposite result.".to_string()],
        }])
    }
}

#[test]
fn a_contradiction_is_verified_and_printed_in_the_disputed_section() {
    let store = Store::new(&scratch::unique("research-contradiction"));
    let web = FixtureWeb::new();
    let planner = OneStepPlanner;
    let extractor = ContradictionExtractor;
    let engine = Engine {
        store: &store,
        web: &web,
        planner: &planner,
        extractor: &extractor,
        mode: RunMode::Web,
    };
    let run = engine
        .run_named(
            "r-contradiction",
            "What happened in the alpha fixture?",
            ResearchBudget {
                queries: 2,
                fetches: 2,
                minutes: 5,
                claims_per_source: 2,
            },
        )
        .expect("a completed run");
    assert_eq!(run.status, RunStatus::Published, "{:?}", run.failures);
    assert_eq!(run.claims[0].contradict.len(), 1);
    let report =
        fs::read_to_string(store.dir("r-contradiction").join("report.md")).expect("a report");
    assert!(report.contains("## Disputed"), "{report}");
    assert!(report.contains("A contradicting observation"), "{report}");
    assert_eq!(
        check(&store, "r-contradiction").expect("a check").code,
        harness::research::CheckCode::Green
    );
}

#[test]
fn the_bench_runs_a_question_set_and_reports_coverage() {
    let store = Store::new(&scratch::unique("research-bench"));
    let web = FixtureWeb::new();
    let planner = FixedPlanner;
    let extractor = FixedExtractor;
    let questions = vec![BenchQuestion {
        id: "q1".to_string(),
        question: "What happened in the two fixture reports?".to_string(),
        key_terms: vec!["alpha".to_string(), "beta".to_string()],
        min_claims: 2,
    }];
    let rows = run_bench(
        &store,
        &web,
        &planner,
        &extractor,
        RunMode::Web,
        &questions,
        ResearchBudget {
            queries: 4,
            fetches: 4,
            minutes: 5,
            claims_per_source: 2,
        },
    )
    .expect("a bench");
    assert_eq!(rows.len(), 1);
    assert!(rows[0].published);
    assert_eq!(rows[0].coverage, 2);
    assert!(rows[0].missing.is_empty());
}
