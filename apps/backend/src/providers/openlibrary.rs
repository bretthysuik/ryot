use anyhow::{anyhow, Result};
use async_trait::async_trait;
use chrono::{Datelike, NaiveDate};
use convert_case::{Case, Casing};
use http_types::mime;
use itertools::Itertools;
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use serde_json::json;
use surf::{http::headers::ACCEPT, Client};
use surf_governor::GovernorMiddleware;
use surf_retry::{ExponentialBackoff, RetryMiddleware};

use crate::{
    config::OpenlibraryConfig,
    migrator::{MetadataLot, MetadataSource},
    models::{
        media::{
            BookSpecifics, MediaDetails, MediaSearchItem, MediaSpecifics, MetadataCreator,
            MetadataImage, MetadataImageLot, PartialMetadata,
        },
        SearchDetails, SearchResults, StoredUrl,
    },
    traits::{MediaProvider, MediaProviderLanguages},
    utils::get_base_http_client,
};

static URL: &str = "https://openlibrary.org/";
static IMAGE_BASE_URL: &str = "https://covers.openlibrary.org";

#[derive(Serialize, Deserialize, Debug, Clone)]
struct BookSearchResults {
    total: i32,
    items: Vec<BookSearchItem>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct BookSearchItem {
    identifier: String,
    title: String,
    description: Option<String>,
    author_names: Vec<String>,
    genres: Vec<String>,
    images: Vec<String>,
    publish_year: Option<i32>,
    publish_date: Option<NaiveDate>,
    book_specifics: BookSpecifics,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct OpenlibraryKey {
    key: String,
}

#[derive(Debug, Clone)]
pub struct OpenlibraryService {
    image_url: String,
    image_size: String,
    client: Client,
    page_limit: i32,
}

impl MediaProviderLanguages for OpenlibraryService {
    fn supported_languages() -> Vec<String> {
        ["us"].into_iter().map(String::from).collect()
    }

    fn default_language() -> String {
        "us".to_owned()
    }
}

impl OpenlibraryService {
    pub async fn new(config: &OpenlibraryConfig, page_limit: i32) -> Self {
        let client = get_base_http_client(URL, vec![(ACCEPT, mime::JSON)]);
        Self {
            image_url: IMAGE_BASE_URL.to_owned(),
            image_size: config.cover_image_size.to_string(),
            client,
            page_limit,
        }
    }
}

#[async_trait]
impl MediaProvider for OpenlibraryService {
    async fn details(&self, identifier: &str) -> Result<MediaDetails> {
        #[derive(Debug, Serialize, Deserialize, Clone)]
        struct OpenlibraryAuthor {
            author: OpenlibraryKey,
            #[serde(rename = "type", flatten)]
            role: Option<OpenlibraryKey>,
        }
        #[derive(Debug, Serialize, Deserialize, Clone)]
        #[serde(untagged)]
        enum OpenlibraryAuthorResponse {
            Flat(OpenlibraryKey),
            Nested(OpenlibraryAuthor),
        }
        #[derive(Debug, Serialize, Deserialize, Clone)]
        #[serde(untagged)]
        enum OpenlibraryDescription {
            Text(String),
            Nested {
                #[serde(rename = "type")]
                key: String,
                value: String,
            },
        }
        #[derive(Debug, Serialize, Deserialize, Clone)]
        struct OpenlibraryBook {
            key: String,
            description: Option<OpenlibraryDescription>,
            title: String,
            covers: Option<Vec<i64>>,
            authors: Option<Vec<OpenlibraryAuthorResponse>>,
            subjects: Option<Vec<String>>,
        }
        let mut rsp = self
            .client
            .get(format!("works/{}.json", identifier))
            .await
            .map_err(|e| anyhow!(e))?;

        let data: OpenlibraryBook = rsp.body_json().await.map_err(|e| anyhow!(e))?;

        let identifier = get_key(&data.key);

        #[derive(Debug, Serialize, Deserialize, Clone)]
        struct OpenlibraryEdition {
            publish_date: Option<String>,
            number_of_pages: Option<i32>,
            covers: Option<Vec<i64>>,
        }
        #[derive(Debug, Serialize, Deserialize, Clone)]
        struct OpenlibraryEditionsResponse {
            entries: Option<Vec<OpenlibraryEdition>>,
        }
        let mut rsp = self
            .client
            .get(format!("works/{}/editions.json", identifier))
            .await
            .map_err(|e| anyhow!(e))?;
        let editions: OpenlibraryEditionsResponse =
            rsp.body_json().await.map_err(|e| anyhow!(e))?;

        let entries = editions.entries.unwrap_or_default();
        let all_pages = entries
            .iter()
            .filter_map(|f| f.number_of_pages)
            .collect_vec();
        let num_pages = if all_pages.is_empty() {
            0
        } else {
            all_pages.iter().sum::<i32>() / all_pages.len() as i32
        };
        let first_release_date = entries
            .iter()
            .filter_map(|f| f.publish_date.clone())
            .filter_map(|f| Self::parse_date(&f))
            .min();

        #[derive(Debug, Serialize, Deserialize)]
        struct OpenlibraryAuthorPartial {
            name: String,
            photos: Option<Vec<i64>>,
        }
        let mut creators = vec![];
        for a in data.authors.unwrap_or_default().iter() {
            let (key, role) = match a {
                OpenlibraryAuthorResponse::Flat(s) => (s.key.to_owned(), "Author".to_owned()),
                OpenlibraryAuthorResponse::Nested(s) => (
                    s.author.key.to_owned(),
                    s.role
                        .as_ref()
                        .map(|r| r.key.clone())
                        .unwrap_or_else(|| "Author".to_owned()),
                ),
            };
            let mut rsp = self
                .client
                .get(format!("{}.json", key))
                .await
                .map_err(|e| anyhow!(e))?;
            let OpenlibraryAuthorPartial { name, photos } =
                rsp.body_json().await.map_err(|e| anyhow!(e))?;
            let image = photos
                .unwrap_or_default()
                .into_iter()
                .filter(|c| c > &0)
                .collect_vec()
                .first()
                .map(|i| self.get_author_cover_image_url(*i));
            creators.push(MetadataCreator { name, role, image });
        }
        let description = data.description.map(|d| match d {
            OpenlibraryDescription::Text(s) => s,
            OpenlibraryDescription::Nested { value, .. } => value,
        });

        let mut images = vec![];
        for c in data.covers.iter().flatten() {
            images.push(*c);
        }
        for c in entries
            .iter()
            .flat_map(|e| e.covers.to_owned().unwrap_or_default())
        {
            images.push(c);
        }

        let images = images
            .into_iter()
            .filter(|c| c > &0)
            .map(|c| MetadataImage {
                url: StoredUrl::Url(self.get_book_cover_image_url(c)),
                lot: MetadataImageLot::Poster,
            })
            .unique()
            .collect();

        let genres = data
            .subjects
            .unwrap_or_default()
            .into_iter()
            .flat_map(|s| s.split(", ").map(|d| d.to_case(Case::Title)).collect_vec())
            .collect_vec();

        #[derive(Debug, Serialize, Deserialize)]
        struct OpenlibraryPartialResponse {
            #[serde(rename = "0")]
            data: String,
        }

        // DEV: Reverse engineered the API
        let html = self
            .client
            .get("partials.json")
            .query(&json!({ "workid": identifier, "_component": "RelatedWorkCarousel" }))
            .unwrap()
            .await
            .map_err(|e| anyhow!(e))?
            .body_json::<OpenlibraryPartialResponse>()
            .await
            .map_err(|e| anyhow!(e))?
            .data;

        let mut suggestions = vec![];

        let fragment = Html::parse_document(&html);

        let carousel_item_selector = Selector::parse(".book.carousel__item").unwrap();
        let image_selector = Selector::parse("img.bookcover").unwrap();
        let identifier_selector = Selector::parse("a[href]").unwrap();

        for item in fragment.select(&carousel_item_selector) {
            let identifier = get_key(
                &item
                    .select(&identifier_selector)
                    .next()
                    .and_then(|a| a.value().attr("href"))
                    .map(|href| href.to_string())
                    .unwrap(),
            );
            if let Some(n) = item
                .select(&image_selector)
                .next()
                .and_then(|img| img.value().attr("alt"))
                .map(|alt| alt.to_string())
            {
                let name = n
                    .split(" by ")
                    .next()
                    .map(|name| name.trim().to_string())
                    .unwrap();
                let image = item
                    .select(&image_selector)
                    .next()
                    .and_then(|img| img.value().attr("src"))
                    .map(|src| src.to_string());
                suggestions.push(PartialMetadata {
                    title: name,
                    image,
                    identifier,
                    lot: MetadataLot::Book,
                    source: MetadataSource::Openlibrary,
                });
            }
        }

        Ok(MediaDetails {
            identifier: get_key(&data.key),
            title: data.title,
            production_status: "Released".to_owned(),
            description,
            lot: MetadataLot::Book,
            source: MetadataSource::Openlibrary,
            creators,
            genres,
            images,
            publish_year: first_release_date.map(|d| d.year()),
            specifics: MediaSpecifics::Book(BookSpecifics {
                pages: Some(num_pages),
            }),
            suggestions,
            publish_date: None,
            provider_rating: None,
            videos: vec![],
            groups: vec![],
            is_nsfw: None,
        })
    }

    async fn search(
        &self,
        query: &str,
        page: Option<i32>,
        _display_nsfw: bool,
    ) -> Result<SearchResults<MediaSearchItem>> {
        let page = page.unwrap_or(1);
        #[derive(Debug, Serialize, Deserialize)]
        pub struct OpenlibraryBook {
            key: String,
            title: String,
            author_name: Option<Vec<String>>,
            cover_i: Option<i64>,
            publish_year: Option<Vec<i32>>,
            first_publish_year: Option<i32>,
            number_of_pages_median: Option<i32>,
        }
        #[derive(Serialize, Deserialize, Debug)]
        struct OpenLibrarySearchResponse {
            num_found: i32,
            docs: Vec<OpenlibraryBook>,
        }
        let fields = [
            "key",
            "title",
            "author_name",
            "cover_i",
            "first_publish_year",
        ]
        .join(",");
        let mut rsp = self
            .client
            .get("search.json")
            .query(&json!({
                "q": query.to_owned(),
                "fields": fields,
                "offset": (page - 1) * self.page_limit,
                "limit": self.page_limit,
                "type": "work".to_owned(),
            }))
            .unwrap()
            .await
            .map_err(|e| anyhow!(e))?;
        let search: OpenLibrarySearchResponse = rsp.body_json().await.map_err(|e| anyhow!(e))?;
        let resp = search
            .docs
            .into_iter()
            .map(|d| {
                let images = Vec::from_iter(d.cover_i.map(|f| self.get_book_cover_image_url(f)));
                BookSearchItem {
                    identifier: get_key(&d.key),
                    title: d.title,
                    description: None,
                    author_names: d.author_name.unwrap_or_default(),
                    genres: vec![],
                    publish_year: d.first_publish_year,
                    publish_date: None,
                    book_specifics: BookSpecifics {
                        pages: d.number_of_pages_median,
                    },
                    images,
                }
            })
            .collect_vec();
        let data = BookSearchResults {
            total: search.num_found,
            items: resp,
        };
        let next_page = if search.num_found - ((page) * self.page_limit) > 0 {
            Some(page + 1)
        } else {
            None
        };
        Ok(SearchResults {
            details: SearchDetails {
                total: data.total,
                next_page,
            },
            items: data
                .items
                .into_iter()
                .map(|b| MediaSearchItem {
                    identifier: b.identifier,
                    title: b.title,
                    image: b.images.get(0).cloned(),
                    publish_year: b.publish_year,
                })
                .collect(),
        })
    }
}

impl OpenlibraryService {
    fn get_book_cover_image_url(&self, c: i64) -> String {
        self.get_cover_image_url("b", c)
    }

    fn get_author_cover_image_url(&self, c: i64) -> String {
        self.get_cover_image_url("a", c)
    }

    fn get_cover_image_url(&self, t: &str, c: i64) -> String {
        format!(
            "{}/{}/id/{}-{}.jpg?default=false",
            self.image_url, t, c, self.image_size
        )
    }

    fn parse_date(input: &str) -> Option<NaiveDate> {
        let formats = ["%b %d, %Y", "%Y", "%b %d, %Y"];
        for format in formats.iter() {
            if let Ok(date) = NaiveDate::parse_from_str(input, format) {
                return Some(date);
            }
        }
        None
    }

    /// Get a book's ID from its ISBN
    pub async fn id_from_isbn(&self, isbn: &str) -> Option<String> {
        let mut resp = self
            .client
            .clone()
            .with(GovernorMiddleware::per_second(1).ok()?)
            .with(RetryMiddleware::new(
                3,
                ExponentialBackoff::builder().build_with_max_retries(3),
                1,
            ))
            .get(format!("isbn/{}.json", isbn))
            .await
            .ok()?;
        #[derive(Debug, Serialize, Deserialize, Clone)]
        struct Response {
            works: Vec<OpenlibraryKey>,
        }
        let details: Response = resp.body_json().await.ok()?;
        details.works.first().map(|k| get_key(&k.key))
    }
}

pub fn get_key(key: &str) -> String {
    key.split('/')
        .collect_vec()
        .last()
        .cloned()
        .unwrap()
        .to_owned()
}
