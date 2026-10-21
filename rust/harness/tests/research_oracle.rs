//! End-to-end proof for the research core: a fixture web, a two-step plan, two stateless
//! extractors, one stored source each, and a deterministic provenance check.

use std::collections::BTreeMap;
use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};

use harness::research::{
    BenchGrade, BenchQuestion, BenchVerdict, ClaimDraft, ClaimKind, Engine, Extractor, Fetched,
    PlanItem, Planner, ResearchBudget, RunMode, RunStatus, SearchHit, Source, Store, Web, check,
    run_bench,
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
        judge: None,
        entailment: None,
        workers: 1,
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
        judge: None,
        entailment: None,
        workers: 1,
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
        judge: None,
        entailment: None,
        workers: 1,
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
        judge: None,
        entailment: None,
        workers: 1,
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

struct TwoSourceWeb;

impl Web for TwoSourceWeb {
    fn search(&self, _query: &str, _limit: usize) -> Result<Vec<SearchHit>, String> {
        Ok(vec![
            SearchHit {
                title: "Alpha report".to_string(),
                url: "https://example.com/alpha".to_string(),
                snippet: String::new(),
            },
            SearchHit {
                title: "Beta report".to_string(),
                url: "https://example.com/beta".to_string(),
                snippet: String::new(),
            },
        ])
    }

    fn fetch(&self, url: &str) -> Result<Fetched, String> {
        let body = if url.ends_with("alpha") {
            format!(
                "Alpha report. Reading A: the measured value is 10 units. {}",
                "The first source explains the mechanism in enough detail to be useful. ".repeat(5)
            )
        } else {
            format!(
                "Beta report. Reading B: the measured value is 20 units. {}",
                "The second source adds a separate observation with its own evidence. ".repeat(5)
            )
        };
        Ok(Fetched {
            url: url.to_string(),
            final_url: url.to_string(),
            title: "fixture".to_string(),
            status: Some(200),
            content_type: "text/html".to_string(),
            body,
            excerpt: None,
        })
    }
}

struct OneQuestion;

impl Planner for OneQuestion {
    fn plan(&self, _question: &str, _budget: &ResearchBudget) -> Result<Vec<PlanItem>, String> {
        Ok(vec![PlanItem {
            text: "What value did the sources measure?".to_string(),
            query: "measured value".to_string(),
            kind: harness::research::QuestionKind::Fact,
        }])
    }
}

struct ConflictingExtractor;

impl Extractor for ConflictingExtractor {
    fn extract(
        &self,
        _subquestion: &str,
        source: &Source,
        body: &str,
        _budget: &ResearchBudget,
    ) -> Result<Vec<ClaimDraft>, String> {
        let (text, quote) = if body.contains("Reading A") {
            (
                "The measured value is 10 units.".to_string(),
                "Reading A: the measured value is 10 units.",
            )
        } else {
            (
                "The measured value is 20 units.".to_string(),
                "Reading B: the measured value is 20 units.",
            )
        };
        let _ = source;
        Ok(vec![ClaimDraft {
            text,
            quote: quote.to_string(),
            kind: ClaimKind::Sourced,
            decision: None,
            alternatives: Vec::new(),
            contradicts: Vec::new(),
        }])
    }
}

struct AlwaysConflicting;

impl harness::research::DisputeJudge for AlwaysConflicting {
    fn judge(
        &self,
        _left: &harness::research::Claim,
        _right: &harness::research::Claim,
    ) -> Result<Option<String>, String> {
        Ok(Some(
            "the sources report different measured values".to_string(),
        ))
    }
}

#[test]
fn a_cross_source_dispute_is_recorded_in_the_run_and_the_report() {
    let store = Store::new(&scratch::unique("research-judge"));
    let web = TwoSourceWeb;
    let planner = OneQuestion;
    let extractor = ConflictingExtractor;
    let judge = AlwaysConflicting;
    let engine = Engine {
        store: &store,
        web: &web,
        planner: &planner,
        extractor: &extractor,
        judge: Some(&judge),
        entailment: None,
        workers: 1,
        mode: RunMode::Web,
    };
    let run = engine
        .run_named(
            "r-judge",
            "What value did the sources measure?",
            ResearchBudget {
                queries: 1,
                fetches: 2,
                minutes: 5,
                claims_per_source: 1,
            },
        )
        .expect("a completed run");
    assert_eq!(run.status, RunStatus::Published, "{:?}", run.failures);
    assert_eq!(run.claims.len(), 2);
    assert_eq!(run.disputes.len(), 1);
    assert_eq!(run.disputes[0].left, "c1");
    assert_eq!(run.disputes[0].right, "c2");
    let report = fs::read_to_string(store.dir("r-judge").join("report.md")).expect("a report");
    assert!(report.contains("## Cross-source disputes"), "{report}");
    assert!(report.contains("[d1] [c1] vs [c2]"), "{report}");
    let json = fs::read_to_string(store.dir("r-judge").join("report.json")).expect("a report");
    assert!(json.contains("\"disputes\""), "{json}");
}

#[test]
fn bench_grades_compare_coverage_with_a_human_verdict() {
    let store = Store::new(&scratch::unique("research-bench-grades"));
    let web = FixtureWeb::new();
    let planner = FixedPlanner;
    let extractor = FixedExtractor;
    let mut questions = Vec::new();
    for id in ["q1", "q2", "q3", "q4"] {
        questions.push(BenchQuestion {
            id: id.to_string(),
            question: "What happened in the two fixture reports?".to_string(),
            key_terms: vec!["alpha".to_string()],
            min_claims: 1,
        });
    }
    questions.push(BenchQuestion {
        id: "q5".to_string(),
        question: "What happened in the two fixture reports?".to_string(),
        key_terms: vec!["alpha".to_string(), "not-present".to_string()],
        min_claims: 1,
    });
    let grades = vec![
        BenchGrade {
            id: "q1".to_string(),
            verdict: BenchVerdict::Pass,
            notes: String::new(),
        },
        BenchGrade {
            id: "q2".to_string(),
            verdict: BenchVerdict::Pass,
            notes: String::new(),
        },
        BenchGrade {
            id: "q3".to_string(),
            verdict: BenchVerdict::Pass,
            notes: String::new(),
        },
        BenchGrade {
            id: "q4".to_string(),
            verdict: BenchVerdict::Pass,
            notes: String::new(),
        },
        BenchGrade {
            id: "q5".to_string(),
            verdict: BenchVerdict::Partial,
            notes: "one term is absent".to_string(),
        },
    ];
    let rows = harness::research::run_bench_with_grades(
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
        Some(&grades),
    )
    .expect("a graded bench");
    assert_eq!(rows.len(), 5);
    assert_eq!(
        rows.iter()
            .filter(|row| row.agreement == Some(true))
            .count(),
        5,
        "the coverage bands match the grades: {:?}",
        rows.iter()
            .map(|row| (&row.id, &row.grade, row.agreement))
            .collect::<Vec<_>>()
    );
    assert_eq!(rows[4].grade.as_deref(), Some("partial"));
}

#[test]
fn a_grade_for_an_unknown_question_is_a_named_error() {
    let store = Store::new(&scratch::unique("research-bench-missing-grade"));
    let web = FixtureWeb::new();
    let planner = FixedPlanner;
    let extractor = FixedExtractor;
    let questions = vec![BenchQuestion {
        id: "q1".to_string(),
        question: "What happened in the two fixture reports?".to_string(),
        key_terms: vec!["alpha".to_string()],
        min_claims: 1,
    }];
    let grades = vec![BenchGrade {
        id: "q9".to_string(),
        verdict: BenchVerdict::Pass,
        notes: String::new(),
    }];
    let error = harness::research::run_bench_with_grades(
        &store,
        &web,
        &planner,
        &extractor,
        RunMode::Web,
        &questions,
        ResearchBudget {
            queries: 1,
            fetches: 1,
            minutes: 5,
            claims_per_source: 1,
        },
        Some(&grades),
    )
    .expect_err("a grade for a missing question is an error");
    assert!(error.contains("q9"), "{error}");
}

struct SocialWeb;

impl Web for SocialWeb {
    fn search(&self, _query: &str, _limit: usize) -> Result<Vec<SearchHit>, String> {
        Ok(vec![SearchHit {
            title: "A post".to_string(),
            url: "https://x.com/lab/status/1".to_string(),
            snippet: String::new(),
        }])
    }

    fn fetch(&self, url: &str) -> Result<Fetched, String> {
        if url.contains("x.com") {
            return Err("the engine must use fetch_raw for a post".to_string());
        }
        Ok(Fetched {
            url: url.to_string(),
            final_url: url.to_string(),
            title: "The paper".to_string(),
            status: Some(200),
            content_type: "text/html".to_string(),
            body: format!(
                "The measured value is 410 tokens per second. {}",
                "The artifact explains the method and the condition in enough detail. ".repeat(7)
            ),
            excerpt: None,
        })
    }

    fn fetch_raw(&self, url: &str) -> Result<String, String> {
        if !url.contains("x.com") {
            return Err("not a post".to_string());
        }
        Ok(r#"<html><a href="/home">home</a><a href="https://arxiv.org/abs/2610.23839">paper</a></html>"#.to_string())
    }
}

struct NoArtifactWeb;

impl Web for NoArtifactWeb {
    fn search(&self, _query: &str, _limit: usize) -> Result<Vec<SearchHit>, String> {
        Ok(vec![SearchHit {
            title: "A post".to_string(),
            url: "https://x.com/lab/status/2".to_string(),
            snippet: String::new(),
        }])
    }

    fn fetch(&self, _url: &str) -> Result<Fetched, String> {
        Err("the post has no artifact".to_string())
    }

    fn fetch_raw(&self, _url: &str) -> Result<String, String> {
        Ok(r#"<a href="/home">home</a><a href="https://t.co/abc">short</a>"#.to_string())
    }
}

struct SocialExtractor;

impl Extractor for SocialExtractor {
    fn extract(
        &self,
        _subquestion: &str,
        _source: &Source,
        _body: &str,
        _budget: &ResearchBudget,
    ) -> Result<Vec<ClaimDraft>, String> {
        Ok(vec![ClaimDraft {
            text: "The measured value is 410 tokens per second.".to_string(),
            quote: "The measured value is 410 tokens per second.".to_string(),
            kind: ClaimKind::Sourced,
            decision: None,
            alternatives: Vec::new(),
            contradicts: Vec::new(),
        }])
    }
}

struct TwoPostWeb {
    fetched: AtomicUsize,
}

impl Web for TwoPostWeb {
    fn search(&self, _query: &str, _limit: usize) -> Result<Vec<SearchHit>, String> {
        Ok((1..=2)
            .map(|index| SearchHit {
                title: format!("Post {index}"),
                url: format!("https://x.com/lab/status/{index}"),
                snippet: String::new(),
            })
            .collect())
    }

    fn fetch(&self, url: &str) -> Result<Fetched, String> {
        self.fetched.fetch_add(1, Ordering::SeqCst);
        Ok(Fetched {
            url: url.to_string(),
            final_url: url.to_string(),
            title: "The paper".to_string(),
            status: Some(200),
            content_type: "text/html".to_string(),
            body: format!(
                "The measured value is 410 tokens per second. {}",
                "The artifact explains the method and the condition in enough detail. ".repeat(7)
            ),
            excerpt: None,
        })
    }

    fn fetch_raw(&self, _url: &str) -> Result<String, String> {
        Ok(r#"<a href="https://arxiv.org/abs/2610.23839">paper</a>"#.to_string())
    }
}

struct FixedAuditor;

impl harness::research::EntailmentAuditor for FixedAuditor {
    fn audit(
        &self,
        claim: &harness::research::Claim,
        _quote: &str,
    ) -> Result<(String, Option<harness::task::Usage>), String> {
        let label = if claim.id == "c1" {
            "partial: the quote drops the batch size"
        } else {
            "unsupported: the quote does not establish the claim"
        };
        Ok((label.to_string(), None))
    }
}

struct SlowWeb {
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
}

impl SlowWeb {
    fn new() -> SlowWeb {
        SlowWeb {
            in_flight: AtomicUsize::new(0),
            max_in_flight: AtomicUsize::new(0),
        }
    }
}

impl Web for SlowWeb {
    fn search(&self, _query: &str, _limit: usize) -> Result<Vec<SearchHit>, String> {
        Ok((1..=4)
            .map(|index| SearchHit {
                title: format!("Source {index}"),
                url: format!("https://example.com/{index}"),
                snippet: String::new(),
            })
            .collect())
    }

    fn fetch(&self, url: &str) -> Result<Fetched, String> {
        let in_flight = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_in_flight.fetch_max(in_flight, Ordering::SeqCst);
        std::thread::sleep(std::time::Duration::from_millis(100));
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        Ok(Fetched {
            url: url.to_string(),
            final_url: url.to_string(),
            title: "Source".to_string(),
            status: Some(200),
            content_type: "text/html".to_string(),
            body: format!(
                "The measured value is 410 tokens per second. {}",
                "The source explains the method and the condition in enough detail. ".repeat(7)
            ),
            excerpt: None,
        })
    }
}

#[test]
fn a_social_post_supplies_its_linked_artifact_as_the_evidence() {
    let store = Store::new(&scratch::unique("research-social"));
    let web = SocialWeb;
    let planner = OneQuestion;
    let extractor = SocialExtractor;
    let engine = Engine {
        store: &store,
        web: &web,
        planner: &planner,
        extractor: &extractor,
        judge: None,
        entailment: None,
        workers: 1,
        mode: RunMode::Web,
    };
    let run = engine
        .run_named(
            "r-social",
            "What value did the source measure?",
            ResearchBudget {
                queries: 1,
                fetches: 4,
                minutes: 5,
                claims_per_source: 1,
            },
        )
        .expect("a published social run");
    assert_eq!(run.status, RunStatus::Published, "{:?}", run.failures);
    assert_eq!(run.sources.len(), 1);
    let source = &run.sources[0];
    assert_eq!(source.url, "https://x.com/lab/status/1");
    assert_eq!(source.final_url, "https://arxiv.org/abs/2610.23839");
    assert_eq!(
        source.artifact_url.as_deref(),
        Some("https://arxiv.org/abs/2610.23839")
    );
    assert_eq!(source.kind, harness::research::SourceKind::Primary);
    let report = fs::read_to_string(store.dir("r-social").join("report.md")).expect("a report");
    assert!(
        report.contains("https://arxiv.org/abs/2610.23839"),
        "{report}"
    );
    assert!(
        report.contains("via https://x.com/lab/status/1"),
        "{report}"
    );
}

#[test]
fn a_duplicate_artifact_is_not_fetched_twice() {
    let store = Store::new(&scratch::unique("research-social-duplicate"));
    let web = TwoPostWeb {
        fetched: AtomicUsize::new(0),
    };
    let planner = OneQuestion;
    let extractor = SocialExtractor;
    let engine = Engine {
        store: &store,
        web: &web,
        planner: &planner,
        extractor: &extractor,
        judge: None,
        entailment: None,
        workers: 1,
        mode: RunMode::Web,
    };
    let run = engine
        .run_named(
            "r-social-duplicate",
            "What value did the source measure?",
            ResearchBudget {
                queries: 1,
                fetches: 6,
                minutes: 5,
                claims_per_source: 1,
            },
        )
        .expect("one artifact is enough");
    assert_eq!(run.sources.len(), 1);
    assert_eq!(web.fetched.load(Ordering::SeqCst), 1);
    assert!(
        run.failures
            .iter()
            .any(|failure| failure.reason.contains("already fetched")),
        "{:?}",
        run.failures
    );
}

#[test]
fn a_social_post_without_an_artifact_is_a_gate_failure() {
    let store = Store::new(&scratch::unique("research-social-empty"));
    let web = NoArtifactWeb;
    let planner = OneQuestion;
    let extractor = SocialExtractor;
    let engine = Engine {
        store: &store,
        web: &web,
        planner: &planner,
        extractor: &extractor,
        judge: None,
        entailment: None,
        workers: 1,
        mode: RunMode::Web,
    };
    let error = engine
        .run_named(
            "r-social-empty",
            "What value did the source measure?",
            ResearchBudget {
                queries: 1,
                fetches: 4,
                minutes: 5,
                claims_per_source: 1,
            },
        )
        .expect_err("a post with no artifact cannot publish");
    assert!(error.contains("no verified claim"), "{error}");
    let run = store.load("r-social-empty").expect("the failed run");
    assert!(
        run.failures
            .iter()
            .any(|failure| failure.stage == "gate" && failure.reason.contains("no linked artifact")),
        "{:?}",
        run.failures
    );
}

#[test]
fn an_entailment_audit_is_reported_without_changing_the_check() {
    let store = Store::new(&scratch::unique("research-entailment"));
    let web = FixtureWeb::new();
    let planner = FixedPlanner;
    let extractor = FixedExtractor;
    let auditor = FixedAuditor;
    let engine = Engine {
        store: &store,
        web: &web,
        planner: &planner,
        extractor: &extractor,
        judge: None,
        entailment: Some(&auditor),
        workers: 1,
        mode: RunMode::Web,
    };
    let run = engine
        .run_named(
            "r-entailment",
            "What happened in the two fixture reports?",
            ResearchBudget {
                queries: 4,
                fetches: 4,
                minutes: 5,
                claims_per_source: 2,
            },
        )
        .expect("a completed run");
    assert_eq!(run.status, RunStatus::Published, "{:?}", run.failures);
    assert_eq!(
        run.entailments.get("c1").map(String::as_str),
        Some("partial: the quote drops the batch size")
    );
    assert_eq!(
        run.entailments.get("c2").map(String::as_str),
        Some("unsupported: the quote does not establish the claim")
    );
    let report = fs::read_to_string(store.dir("r-entailment").join("report.md")).expect("a report");
    assert!(report.contains("Entailment: partial"), "{report}");
    assert!(
        check(&store, "r-entailment").expect("a check").code == harness::research::CheckCode::Green
    );
}

#[test]
fn parallel_fetches_overlap_and_sequential_fetches_do_not() {
    let store = Store::new(&scratch::unique("research-workers"));
    let web = SlowWeb::new();
    let planner = OneQuestion;
    let extractor = SocialExtractor;
    let engine = Engine {
        store: &store,
        web: &web,
        planner: &planner,
        extractor: &extractor,
        judge: None,
        entailment: None,
        workers: 4,
        mode: RunMode::Web,
    };
    let run = engine
        .run_named(
            "r-workers-4",
            "What value did the source measure?",
            ResearchBudget {
                queries: 1,
                fetches: 4,
                minutes: 5,
                claims_per_source: 1,
            },
        )
        .expect("a parallel run");
    assert_eq!(run.status, RunStatus::Published, "{:?}", run.failures);
    assert!(
        web.max_in_flight.load(Ordering::SeqCst) >= 2,
        "max in flight was {}",
        web.max_in_flight.load(Ordering::SeqCst)
    );
    let order: Vec<&str> = run
        .sources
        .iter()
        .map(|source| source.url.as_str())
        .collect();
    assert_eq!(
        order,
        vec![
            "https://example.com/1",
            "https://example.com/2",
            "https://example.com/3",
            "https://example.com/4"
        ]
    );

    let store = Store::new(&scratch::unique("research-workers-one"));
    let web = SlowWeb::new();
    let engine = Engine {
        store: &store,
        web: &web,
        planner: &planner,
        extractor: &extractor,
        judge: None,
        entailment: None,
        workers: 1,
        mode: RunMode::Web,
    };
    engine
        .run_named(
            "r-workers-1",
            "What value did the source measure?",
            ResearchBudget {
                queries: 1,
                fetches: 4,
                minutes: 5,
                claims_per_source: 1,
            },
        )
        .expect("a sequential run");
    assert_eq!(
        web.max_in_flight.load(Ordering::SeqCst),
        1,
        "workers=1 must not overlap fetches"
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
