//! What a manga search asks MangaDex for: a title (or none, to browse), filters and a sort
//! order.

use serde::Deserialize;

use super::models::LocalizedString;
use super::{ContentRating, Query, SEARCH_PAGE_SIZE};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Sort {
    /// Best match for the title; most followed when browsing without one.
    #[default]
    Relevance,
    MostFollowed,
    HighestRated,
    LatestUpload,
    RecentlyAdded,
    Title,
    Year,
}

impl Sort {
    pub const ALL: [Sort; 7] = [
        Sort::Relevance,
        Sort::MostFollowed,
        Sort::HighestRated,
        Sort::LatestUpload,
        Sort::RecentlyAdded,
        Sort::Title,
        Sort::Year,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Sort::Relevance => "Best match",
            Sort::MostFollowed => "Most followed",
            Sort::HighestRated => "Highest rated",
            Sort::LatestUpload => "Latest upload",
            Sort::RecentlyAdded => "Recently added",
            Sort::Title => "Title A–Z",
            Sort::Year => "Newest first",
        }
    }

    fn order(self, has_title: bool) -> (&'static str, &'static str) {
        match self {
            Sort::Relevance if has_title => ("order[relevance]", "desc"),
            Sort::Relevance | Sort::MostFollowed => ("order[followedCount]", "desc"),
            Sort::HighestRated => ("order[rating]", "desc"),
            Sort::LatestUpload => ("order[latestUploadedChapter]", "desc"),
            Sort::RecentlyAdded => ("order[createdAt]", "desc"),
            Sort::Title => ("order[title]", "asc"),
            Sort::Year => ("order[year]", "desc"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ongoing,
    Completed,
    Hiatus,
    Cancelled,
}

impl Status {
    pub const ALL: [Status; 4] = [
        Status::Ongoing,
        Status::Completed,
        Status::Hiatus,
        Status::Cancelled,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Status::Ongoing => "ongoing",
            Status::Completed => "completed",
            Status::Hiatus => "hiatus",
            Status::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Demographic {
    Shounen,
    Shoujo,
    Seinen,
    Josei,
    /// Manga MangaDex has no demographic for.
    None,
}

impl Demographic {
    pub const ALL: [Demographic; 5] = [
        Demographic::Shounen,
        Demographic::Shoujo,
        Demographic::Seinen,
        Demographic::Josei,
        Demographic::None,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Demographic::Shounen => "shounen",
            Demographic::Shoujo => "shoujo",
            Demographic::Seinen => "seinen",
            Demographic::Josei => "josei",
            Demographic::None => "none",
        }
    }
}

/// A MangaDex tag, like "Action" or "Time Travel".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tag {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagFilter {
    Off,
    Include,
    Exclude,
}

/// Narrows a search. Empty lists mean any status, demographic or tags.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchFilters {
    pub sort: Sort,
    /// In [`Status::ALL`] order, as are demographics.
    pub statuses: Vec<Status>,
    pub demographics: Vec<Demographic>,
    /// The year of first publication.
    pub year: Option<u32>,
    /// Results have all of these tags and none of the excluded ones.
    pub included_tags: Vec<Tag>,
    pub excluded_tags: Vec<Tag>,
}

impl SearchFilters {
    pub fn toggle_status(&mut self, status: Status) {
        toggle(&mut self.statuses, status, &Status::ALL);
    }

    pub fn toggle_demographic(&mut self, demographic: Demographic) {
        toggle(&mut self.demographics, demographic, &Demographic::ALL);
    }

    pub fn tag_filter(&self, tag: &Tag) -> TagFilter {
        if self.included_tags.iter().any(|t| t.id == tag.id) {
            TagFilter::Include
        } else if self.excluded_tags.iter().any(|t| t.id == tag.id) {
            TagFilter::Exclude
        } else {
            TagFilter::Off
        }
    }

    /// Includes the tag, then excludes it, then neither.
    pub fn cycle_tag(&mut self, tag: &Tag) {
        let next = match self.tag_filter(tag) {
            TagFilter::Off => TagFilter::Include,
            TagFilter::Include => TagFilter::Exclude,
            TagFilter::Exclude => TagFilter::Off,
        };
        self.included_tags.retain(|t| t.id != tag.id);
        self.excluded_tags.retain(|t| t.id != tag.id);
        match next {
            TagFilter::Include => self.included_tags.push(tag.clone()),
            TagFilter::Exclude => self.excluded_tags.push(tag.clone()),
            TagFilter::Off => {}
        }
    }
}

/// Adds or removes `item`, keeping `list` in the order of `all`.
fn toggle<T: Copy + PartialEq>(list: &mut Vec<T>, item: T, all: &[T]) {
    if list.contains(&item) {
        list.retain(|known| *known != item);
    } else {
        list.push(item);
        list.sort_by_key(|known| all.iter().position(|a| a == known));
    }
}

/// The `GET /manga` query for a page of results. Without a title it browses everything.
pub(super) fn search_query(
    title: &str,
    filters: &SearchFilters,
    offset: u32,
    include_adult: bool,
) -> Query {
    let mut query: Query = Vec::new();
    if !title.is_empty() {
        query.push(("title", title.to_owned()));
    }
    let (order, direction) = filters.sort.order(!title.is_empty());
    query.extend([
        ("limit", SEARCH_PAGE_SIZE.to_string()),
        ("offset", offset.to_string()),
        (order, direction.to_owned()),
        ("includes[]", "cover_art".to_owned()),
        ("includes[]", "author".to_owned()),
        ("includes[]", "artist".to_owned()),
    ]);
    for rating in ContentRating::allowed(include_adult) {
        query.push(("contentRating[]", rating.as_str().to_owned()));
    }
    for status in &filters.statuses {
        query.push(("status[]", status.as_str().to_owned()));
    }
    for demographic in &filters.demographics {
        query.push(("publicationDemographic[]", demographic.as_str().to_owned()));
    }
    if let Some(year) = filters.year {
        query.push(("year", year.to_string()));
    }
    for tag in &filters.included_tags {
        query.push(("includedTags[]", tag.id.clone()));
    }
    for tag in &filters.excluded_tags {
        query.push(("excludedTags[]", tag.id.clone()));
    }
    query
}

#[derive(Debug, Deserialize)]
pub(super) struct TagAttributes {
    #[serde(deserialize_with = "super::models::localized")]
    name: LocalizedString,
}

impl Tag {
    pub(super) fn new(id: String, attributes: TagAttributes) -> Option<Tag> {
        let name = attributes
            .name
            .get("en")
            .or_else(|| attributes.name.values().next())?
            .clone();
        Some(Tag { id, name })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag(id: &str, name: &str) -> Tag {
        Tag {
            id: id.to_owned(),
            name: name.to_owned(),
        }
    }

    fn values<'a>(query: &'a Query, key: &str) -> Vec<&'a str> {
        query
            .iter()
            .filter(|(k, _)| *k == key)
            .map(|(_, v)| v.as_str())
            .collect()
    }

    #[test]
    fn builds_title_searches_and_browsing() {
        let query = search_query("berserk", &SearchFilters::default(), 50, false);
        assert_eq!(values(&query, "title"), ["berserk"]);
        assert_eq!(values(&query, "order[relevance]"), ["desc"]);
        assert_eq!(values(&query, "offset"), ["50"]);
        assert_eq!(values(&query, "contentRating[]"), ["safe", "suggestive"]);
        assert!(values(&query, "status[]").is_empty());

        // Browsing has nothing to be relevant to, so it lists the most followed.
        let query = search_query("", &SearchFilters::default(), 0, true);
        assert!(values(&query, "title").is_empty());
        assert_eq!(values(&query, "order[followedCount]"), ["desc"]);
        assert!(values(&query, "order[relevance]").is_empty());
        assert_eq!(values(&query, "contentRating[]").len(), 4);
    }

    #[test]
    fn sends_every_filter() {
        let mut filters = SearchFilters {
            sort: Sort::Title,
            year: Some(1989),
            ..SearchFilters::default()
        };
        filters.toggle_status(Status::Hiatus);
        filters.toggle_status(Status::Ongoing);
        filters.toggle_demographic(Demographic::Seinen);
        filters.toggle_demographic(Demographic::None);
        filters.toggle_demographic(Demographic::Seinen);
        let action = tag("t1", "Action");
        let romance = tag("t2", "Romance");
        filters.cycle_tag(&action);
        filters.cycle_tag(&romance);
        filters.cycle_tag(&romance);
        assert_eq!(filters.tag_filter(&action), TagFilter::Include);
        assert_eq!(filters.tag_filter(&romance), TagFilter::Exclude);

        let query = search_query("berserk", &filters, 0, false);
        assert_eq!(values(&query, "order[title]"), ["asc"]);
        assert_eq!(values(&query, "status[]"), ["ongoing", "hiatus"]);
        assert_eq!(values(&query, "publicationDemographic[]"), ["none"]);
        assert_eq!(values(&query, "year"), ["1989"]);
        assert_eq!(values(&query, "includedTags[]"), ["t1"]);
        assert_eq!(values(&query, "excludedTags[]"), ["t2"]);

        filters.cycle_tag(&romance);
        assert_eq!(filters.tag_filter(&romance), TagFilter::Off);
        assert!(filters.excluded_tags.is_empty());
    }
}
