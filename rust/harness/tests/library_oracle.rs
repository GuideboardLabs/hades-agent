//! Offline proof: a local Markdown document becomes a research source with no network call.

use std::fs;

use harness::library::{AddOptions, Library, LibraryWeb};
use harness::research::{
    ClaimDraft, ClaimKind, Engine, Extractor, Fetched, PlanItem, Planner, ResearchBudget, RunMode,
    RunStatus, Source, Store, Web, check, check_with,
};
use harness::scratch;

struct OneQuestion;

impl Planner for OneQuestion {
    fn plan(&self, _question: &str, _budget: &ResearchBudget) -> Result<Vec<PlanItem>, String> {
        Ok(vec![PlanItem {
            text: "What does the paper say about Vulkan on RDNA1?".to_string(),
            query: "Vulkan RDNA1".to_string(),
            kind: Default::default(),
        }])
    }
}

struct QuoteThePaper;

impl Extractor for QuoteThePaper {
    fn extract(
        &self,
        _subquestion: &str,
        _source: &Source,
        body: &str,
        _budget: &ResearchBudget,
    ) -> Result<Vec<ClaimDraft>, String> {
        let quote = "Vulkan on RDNA1 drops prompt processing by forty percent.";
        if !body.contains(quote) {
            return Ok(Vec::new());
        }
        Ok(vec![ClaimDraft {
            text: quote.to_string(),
            quote: quote.to_string(),
            kind: ClaimKind::Sourced,
            decision: None,
            alternatives: Vec::new(),
            contradicts: Vec::new(),
        }])
    }
}

#[test]
fn offline_research_reads_the_library_and_checks_green() {
    let root = scratch::unique("library-e2e");
    let paper = root.join("paper.md");
    fs::write(
        &paper,
        "# Vulkan notes\n\nVulkan on RDNA1 drops prompt processing by forty percent.\n",
    )
    .expect("a paper");
    let library = Library::open(root.join("library")).expect("a library");
    library
        .add_path(&paper, &AddOptions::default())
        .expect("an import");

    let store = Store::new(&root.join("state"));
    let planner = OneQuestion;
    let extractor = QuoteThePaper;
    let web = LibraryWeb::new(root.join("library"));
    let engine = Engine {
        store: &store,
        web: &web,
        planner: &planner,
        extractor: &extractor,
        judge: None,
        mode: RunMode::Offline,
    };
    let run = engine
        .run_named(
            "r-offline",
            "What limits Vulkan on RDNA1?",
            ResearchBudget::default(),
        )
        .expect("a run");
    assert_eq!(run.status, RunStatus::Published, "{:?}", run.failures);
    assert_eq!(run.mode, RunMode::Offline);
    assert_eq!(run.claims.len(), 1);
    assert!(run.sources[0].url.starts_with("library://"));
    let checked = check(&store, "r-offline").expect("a check");
    assert_eq!(
        checked.code,
        harness::research::CheckCode::Green,
        "{checked:?}"
    );
    assert!(checked.off_question == 0, "{checked:?}");
    assert!(
        check_with(&store, "r-offline", true)
            .expect("a strict check")
            .code
            == harness::research::CheckCode::Green
    );
    let report = fs::read_to_string(store.dir("r-offline").join("report.md")).expect("a report");
    assert!(report.contains("mode `offline library`"));
    assert!(report.contains("library://"));
}

#[test]
fn a_library_excerpt_starts_at_the_hit_not_page_one() {
    let root = scratch::unique("library-excerpt");
    let paper = root.join("book.md");
    let mut body = String::from("# A long book\n\n");
    for index in 0..80 {
        body.push_str(&format!(
            "Filler paragraph {index} has enough words to take space and no target.\n\n"
        ));
    }
    body.push_str("The needle is a RDNA1 Vulkan limitation found at the end.\n");
    fs::write(&paper, body).expect("a book");
    let library = Library::open(root.join("library")).expect("a library");
    let report = library
        .add_path(&paper, &AddOptions::default())
        .expect("an import");
    let id = report.added[0].entry.id.clone();
    let hits = library.search("needle", 5).expect("a search");
    assert_eq!(hits.len(), 1);
    let url = format!("library://{id}#c{}", hits[0].ord);
    let fetched: Fetched = LibraryWeb::new(root.join("library"))
        .fetch(&url)
        .expect("a fetch");
    let excerpt = fetched.excerpt.expect("an excerpt");
    assert!(excerpt.contains("needle"), "{}", excerpt);
    assert!(!excerpt.contains("Filler paragraph 0 "));
}
