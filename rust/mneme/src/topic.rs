//! The static topic categories, taken from Foxforge's `VALID_TOPIC_TYPES`.
//!
//! A category is a fixed door. A topic is a general thing under it (`AI Inference`), and a subtopic
//! is a narrower thing under that (`KV Cache`). The category never grows; the tree under it does.
//! Keeping the list closed is what stops a vault from growing five names for the same ethos.

use std::fmt;

/// The 26 categories Foxforge recognises. Keys are lowercase with underscores, exactly as its
/// `VALID_TOPIC_TYPES` spells them, so an imported topic keeps its category by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Category {
    Sports,
    Technical,
    Medical,
    Finance,
    History,
    General,
    Science,
    Math,
    Politics,
    CurrentEvents,
    Underground,
    Business,
    Law,
    Education,
    Travel,
    AnimalCare,
    Food,
    Gaming,
    Books,
    RealEstate,
    Automotive,
    Parenting,
    TvShows,
    Movies,
    Music,
    Art,
}

impl Category {
    pub const ALL: [Category; 26] = [
        Category::Sports,
        Category::Technical,
        Category::Medical,
        Category::Finance,
        Category::History,
        Category::General,
        Category::Science,
        Category::Math,
        Category::Politics,
        Category::CurrentEvents,
        Category::Underground,
        Category::Business,
        Category::Law,
        Category::Education,
        Category::Travel,
        Category::AnimalCare,
        Category::Food,
        Category::Gaming,
        Category::Books,
        Category::RealEstate,
        Category::Automotive,
        Category::Parenting,
        Category::TvShows,
        Category::Movies,
        Category::Music,
        Category::Art,
    ];

    pub fn key(self) -> &'static str {
        match self {
            Category::Sports => "sports",
            Category::Technical => "technical",
            Category::Medical => "medical",
            Category::Finance => "finance",
            Category::History => "history",
            Category::General => "general",
            Category::Science => "science",
            Category::Math => "math",
            Category::Politics => "politics",
            Category::CurrentEvents => "current_events",
            Category::Underground => "underground",
            Category::Business => "business",
            Category::Law => "law",
            Category::Education => "education",
            Category::Travel => "travel",
            Category::AnimalCare => "animal_care",
            Category::Food => "food",
            Category::Gaming => "gaming",
            Category::Books => "books",
            Category::RealEstate => "real_estate",
            Category::Automotive => "automotive",
            Category::Parenting => "parenting",
            Category::TvShows => "tv_shows",
            Category::Movies => "movies",
            Category::Music => "music",
            Category::Art => "art",
        }
    }

    /// The heading a fresh vault writes. The key is for the record; the label is for a person.
    pub fn label(self) -> &'static str {
        match self {
            Category::Sports => "Sports",
            Category::Technical => "Technical",
            Category::Medical => "Medical",
            Category::Finance => "Finance",
            Category::History => "History",
            Category::General => "General",
            Category::Science => "Science",
            Category::Math => "Math",
            Category::Politics => "Politics",
            Category::CurrentEvents => "Current Events",
            Category::Underground => "Underground",
            Category::Business => "Business",
            Category::Law => "Law",
            Category::Education => "Education",
            Category::Travel => "Travel",
            Category::AnimalCare => "Animal Care",
            Category::Food => "Food",
            Category::Gaming => "Gaming",
            Category::Books => "Books",
            Category::RealEstate => "Real Estate",
            Category::Automotive => "Automotive",
            Category::Parenting => "Parenting",
            Category::TvShows => "TV Shows",
            Category::Movies => "Movies",
            Category::Music => "Music",
            Category::Art => "Art",
        }
    }

    /// One line, for the generated taxonomy page and the seeded marker.
    pub fn description(self) -> &'static str {
        match self {
            Category::Sports => {
                "Competition, teams, athletes, results, and the business around them."
            }
            Category::Technical => "Software, hardware, systems, and how they are built and run.",
            Category::Medical => "Health, medicine, biology, and the evidence behind a treatment.",
            Category::Finance => "Money, markets, accounting, investing, and personal finance.",
            Category::History => "What happened, when, and the sources that say so.",
            Category::General => "The honest home for a topic that does not fit a narrower door.",
            Category::Science => "Physics, chemistry, biology, and the methods that test a claim.",
            Category::Math => "Proof, statistics, computation, and formal reasoning.",
            Category::Politics => "Government, policy, elections, and public decisions.",
            Category::CurrentEvents => {
                "News that is still moving, and the record of what was said when."
            }
            Category::Underground => {
                "Subcultures, unofficial scenes, and material outside the mainstream."
            }
            Category::Business => "Companies, strategy, operations, and how work is organised.",
            Category::Law => "Statutes, cases, contracts, and the reasoning courts write down.",
            Category::Education => "Teaching, learning, curricula, and how skill is acquired.",
            Category::Travel => "Places, routes, logistics, and what a visit actually takes.",
            Category::AnimalCare => "Animals, husbandry, veterinary care, and behaviour.",
            Category::Food => "Cooking, ingredients, nutrition, and the culture around a table.",
            Category::Gaming => "Games, rules, design, play, and the industry that makes them.",
            Category::Books => "Writing, publishing, reading, and the argument a book makes.",
            Category::RealEstate => "Property, land, rent, mortgages, and local markets.",
            Category::Automotive => "Cars, repair, engineering, and the economics of driving.",
            Category::Parenting => "Children, family practice, development, and care.",
            Category::TvShows => "Series, episodes, production, and the shape of a season.",
            Category::Movies => "Film, production, criticism, and the business of release.",
            Category::Music => "Composition, performance, production, and the industry.",
            Category::Art => "Visual work, craft, design, criticism, and collections.",
        }
    }

    pub fn parse(key: &str) -> Option<Category> {
        let key = key.trim().to_lowercase();
        Category::ALL
            .into_iter()
            .find(|category| category.key() == key)
    }
}

impl fmt::Display for Category {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.key())
    }
}

/// The generated taxonomy page at the vault root. It is a map, not a note: it carries no area and
/// cannot be filed, which is why it sits beside `charter.md` rather than inside `50 Topics`.
pub fn taxonomy() -> String {
    let mut lines = vec![
        "# Topic taxonomy".to_string(),
        String::new(),
        "The 26 categories are fixed, taken from Foxforge's topic types. A topic is a general thing \
         under one category; a subtopic is a narrower thing under a topic. Every topic, and every \
         project it carries, names its category this way."
            .to_string(),
        String::new(),
    ];
    for category in Category::ALL {
        lines.push(format!("## {} (`{}`)", category.label(), category.key()));
        lines.push(String::new());
        lines.push(category.description().to_string());
        lines.push(String::new());
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::{Category, taxonomy};

    #[test]
    fn the_taxonomy_is_closed_and_every_key_round_trips() {
        assert_eq!(Category::ALL.len(), 26);
        for category in Category::ALL {
            assert_eq!(Category::parse(category.key()), Some(category));
        }
        assert_eq!(
            Category::parse("current_events"),
            Some(Category::CurrentEvents)
        );
        assert_eq!(Category::parse("astrology"), None);
        assert!(taxonomy().contains("## Technical (`technical`)"));
    }
}
