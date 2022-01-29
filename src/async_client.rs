use futures::future::BoxFuture;
use futures::prelude::*;
use futures::{future::try_join_all, try_join};
use reqwest::{header, Client as HttpClient, StatusCode, Url};
use serde::de::DeserializeOwned;

use std::collections::VecDeque;

use super::Error;
use crate::types::*;

/// Asynchronous client for the crates.io API.
#[derive(Clone)]
pub struct Client {
    client: HttpClient,
    rate_limit: std::time::Duration,
    last_request_time: std::sync::Arc<tokio::sync::Mutex<Option<tokio::time::Instant>>>,
    base_url: Url,
}

pub struct CrateStream {
    client: Client,
    filter: CratesQuery,

    closed: bool,
    items: VecDeque<Crate>,
    next_page_fetch: Option<BoxFuture<'static, Result<CratesResponse, Error>>>,
}

impl CrateStream {
    fn new(client: Client, filter: CratesQuery) -> Self {
        Self {
            client,
            filter,
            closed: false,
            items: VecDeque::new(),
            next_page_fetch: None,
        }
    }
}

impl futures::stream::Stream for CrateStream {
    type Item = Result<Crate, Error>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let inner = self.get_mut();

        if inner.closed {
            return std::task::Poll::Ready(None);
        }

        if let Some(krate) = inner.items.pop_front() {
            return std::task::Poll::Ready(Some(Ok(krate)));
        }

        if let Some(mut fut) = inner.next_page_fetch.take() {
            return match fut.poll_unpin(cx) {
                std::task::Poll::Ready(res) => match res {
                    Ok(page) if page.crates.is_empty() => {
                        inner.closed = true;
                        std::task::Poll::Ready(None)
                    }
                    Ok(page) => {
                        let mut iter = page.crates.into_iter();
                        let next = iter.next();
                        inner.items.extend(iter);

                        std::task::Poll::Ready(next.map(Ok))
                    }
                    Err(err) => {
                        inner.closed = true;
                        std::task::Poll::Ready(Some(Err(err)))
                    }
                },
                std::task::Poll::Pending => {
                    inner.next_page_fetch = Some(fut);
                    std::task::Poll::Pending
                }
            };
        }

        let filter = inner.filter.clone();
        inner.filter.page += 1;

        let c = inner.client.clone();
        let mut f = Box::pin(async move { c.crates(filter).await });
        assert!(matches!(f.poll_unpin(cx), std::task::Poll::Pending));
        inner.next_page_fetch = Some(f);

        cx.waker().clone().wake();

        std::task::Poll::Pending
    }
}

impl Client {
    /// Instantiate a new client.
    ///
    /// To respect the offical [Crawler Policy](https://crates.io/policies#crawlers),
    /// you must specify both a descriptive user agent and a rate limit interval.
    ///
    /// At most one request will be executed in the specified duration.
    /// The guidelines suggest 1 per second or less.
    /// (Only one request is executed concurrenly, even if the given Duration is 0).
    ///
    /// Example user agent: `"my_bot (my_bot.com/info)"` or `"my_bot (help@my_bot.com)"`.
    ///
    /// ```rust
    /// # fn f() -> Result<(), crates_io_api::Error> {
    /// let client = crates_io_api::AsyncClient::new(
    ///   "my_bot (help@my_bot.com)",
    ///   std::time::Duration::from_millis(1000),
    /// )?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn new(
        user_agent: &str,
        rate_limit: std::time::Duration,
    ) -> Result<Self, reqwest::header::InvalidHeaderValue> {
        let mut headers = header::HeaderMap::new();
        headers.insert(
            header::USER_AGENT,
            header::HeaderValue::from_str(user_agent)?,
        );

        let client = HttpClient::builder()
            .default_headers(headers)
            .build()
            .unwrap();

        let limiter = std::sync::Arc::new(tokio::sync::Mutex::new(None));

        Ok(Self {
            rate_limit,
            last_request_time: limiter,
            client,
            base_url: Url::parse("https://crates.io/api/v1/").unwrap(),
        })
    }

    async fn get<T: DeserializeOwned>(&self, url: &Url) -> Result<T, Error> {
        let mut lock = self.last_request_time.clone().lock_owned().await;

        if let Some(last_request_time) = lock.take() {
            if last_request_time.elapsed() < self.rate_limit {
                tokio::time::sleep(self.rate_limit - last_request_time.elapsed()).await;
            }
        }

        let time = tokio::time::Instant::now();
        let res = self.client.get(url.clone()).send().await?;

        let result = match res.status() {
            StatusCode::NOT_FOUND => Err(Error::NotFound(super::NotFound {
                url: url.to_string(),
            })),
            StatusCode::FORBIDDEN => {
                let reason = res.text().await.unwrap_or_default();
                Err(Error::PermissionDenied(super::error::PermissionDenied {
                    reason,
                }))
            }
            _ if !res.status().is_success() => {
                Err(Error::from(res.error_for_status().unwrap_err()))
            }
            _ => res.json::<ApiResponse<T>>().await.map_err(Error::from),
        };

        (*lock) = Some(time);

        match result? {
            ApiResponse::Ok(t) => Ok(t),
            ApiResponse::Err(err) => Err(Error::Api(err)),
        }
    }

    /// Retrieve a summary containing crates.io wide information.
    pub async fn summary(&self) -> Result<Summary, Error> {
        let url = self.base_url.join("summary").unwrap();
        self.get(&url).await
    }

    /// Retrieve information of a crate.
    ///
    /// If you require detailed information, consider using [full_crate]().
    pub async fn get_crate(&self, name: &str) -> Result<CrateResponse, Error> {
        let url = self.base_url.join("crates/").unwrap().join(name).unwrap();
        self.get(&url).await
    }

    /// Retrieve download stats for a crate.
    pub async fn crate_downloads(&self, name: &str) -> Result<CrateDownloads, Error> {
        let url = self
            .base_url
            .join(&format!("crates/{}/downloads", name))
            .unwrap();
        self.get(&url).await
    }

    /// Retrieve the owners of a crate.
    pub async fn crate_owners(&self, name: &str) -> Result<Vec<User>, Error> {
        let url = self
            .base_url
            .join(&format!("crates/{}/owners", name))
            .unwrap();
        self.get::<Owners>(&url).await.map(|data| data.users)
    }

    /// Get a single page of reverse dependencies.
    ///
    /// Note: page must be >= 1.
    pub async fn crate_reverse_dependencies_page(
        &self,
        crate_name: &str,
        page: u64,
    ) -> Result<ReverseDependencies, Error> {
        // If page is zero, bump it to 1.
        let page = page.max(1);

        let url = self
            .base_url
            .join(&format!(
                "crates/{0}/reverse_dependencies?per_page=100&page={1}",
                crate_name, page
            ))
            .unwrap();

        let page = self.get::<ReverseDependenciesAsReceived>(&url).await?;

        let mut deps = ReverseDependencies {
            dependencies: Vec::new(),
            meta: Meta { total: 0 },
        };
        deps.meta.total = page.meta.total;
        deps.extend(page);
        Ok(deps)
    }

    /// Load all reverse dependencies of a crate.
    ///
    /// Note: Since the reverse dependency endpoint requires pagination, this
    /// will result in multiple requests if the crate has more than 100 reverse
    /// dependencies.
    pub async fn crate_reverse_dependencies(
        &self,
        crate_name: &str,
    ) -> Result<ReverseDependencies, Error> {
        let mut deps = ReverseDependencies {
            dependencies: Vec::new(),
            meta: Meta { total: 0 },
        };

        for page_number in 1.. {
            let page = self
                .crate_reverse_dependencies_page(crate_name, page_number)
                .await?;
            if page.dependencies.is_empty() {
                break;
            }
            deps.dependencies.extend(page.dependencies);
            deps.meta.total = page.meta.total;
        }

        Ok(deps)
    }

    /// Get the total count of reverse dependencies for a given crate.
    pub async fn crate_reverse_dependency_count(&self, crate_name: &str) -> Result<u64, Error> {
        let page = self.crate_reverse_dependencies_page(crate_name, 1).await?;
        Ok(page.meta.total)
    }

    /// Retrieve the authors for a crate version.
    pub async fn crate_authors(&self, name: &str, version: &str) -> Result<Authors, Error> {
        let url = self
            .base_url
            .join(&format!("crates/{}/{}/authors", name, version))
            .unwrap();
        self.get::<AuthorsResponse>(&url).await.map(|res| Authors {
            names: res.meta.names,
        })
    }

    /// Retrieve the dependencies of a crate version.
    pub async fn crate_dependencies(
        &self,
        name: &str,
        version: &str,
    ) -> Result<Vec<Dependency>, Error> {
        let url = self
            .base_url
            .join(&format!("crates/{}/{}/dependencies", name, version))
            .unwrap();
        self.get::<Dependencies>(&url)
            .await
            .map(|res| res.dependencies)
    }

    async fn full_version(&self, version: Version) -> Result<FullVersion, Error> {
        let authors_fut = self.crate_authors(&version.crate_name, &version.num);
        let deps_fut = self.crate_dependencies(&version.crate_name, &version.num);

        try_join!(authors_fut, deps_fut).map(|(authors, deps)| FullVersion {
            created_at: version.created_at,
            updated_at: version.updated_at,
            dl_path: version.dl_path,
            downloads: version.downloads,
            features: version.features,
            id: version.id,
            num: version.num,
            yanked: version.yanked,
            license: version.license,
            links: version.links,
            readme_path: version.readme_path,

            author_names: authors.names,
            dependencies: deps,
        })
    }

    /// Retrieve all available information for a crate, including download
    /// stats,  owners and reverse dependencies.
    ///
    /// The `all_versions` argument controls the retrieval of detailed version
    /// information.
    /// If false, only the data for the latest version will be fetched, if true,
    /// detailed information for all versions will be available.
    /// Note: Each version requires two extra requests.
    pub async fn full_crate(&self, name: &str, all_versions: bool) -> Result<FullCrate, Error> {
        let krate = self.get_crate(name).await?;
        let versions = if !all_versions {
            self.full_version(krate.versions[0].clone())
                .await
                .map(|v| vec![v])
        } else {
            try_join_all(
                krate
                    .versions
                    .clone()
                    .into_iter()
                    .map(|v| self.full_version(v)),
            )
            .await
        }?;
        let dls_fut = self.crate_downloads(name);
        let owners_fut = self.crate_owners(name);
        let reverse_dependencies_fut = self.crate_reverse_dependencies(name);
        try_join!(dls_fut, owners_fut, reverse_dependencies_fut).map(
            |(dls, owners, reverse_dependencies)| {
                let data = krate.crate_data;
                FullCrate {
                    id: data.id,
                    name: data.name,
                    description: data.description,
                    license: krate.versions[0].license.clone(),
                    documentation: data.documentation,
                    homepage: data.homepage,
                    repository: data.repository,
                    total_downloads: data.downloads,
                    max_version: data.max_version,
                    created_at: data.created_at,
                    updated_at: data.updated_at,
                    categories: krate.categories,
                    keywords: krate.keywords,
                    downloads: dls,
                    owners,
                    reverse_dependencies,
                    versions,
                }
            },
        )
    }

    /// Retrieve a page of crates, optionally constrained by a query.
    ///
    /// If you want to get all results without worrying about paging,
    /// use [`all_crates`].
    pub async fn crates(&self, query: CratesQuery) -> Result<CratesResponse, Error> {
        let mut url = self.base_url.join("crates").unwrap();
        query.build(url.query_pairs_mut());
        self.get(&url).await
    }

    pub fn crates_stream(&self, filter: CratesQuery) -> CrateStream {
        CrateStream::new(self.clone(), filter)
    }

    /// Retrieves a user by username.
    pub async fn user(&self, username: &str) -> Result<User, Error> {
        let url = self.base_url.join(&format!("users/{}", username)).unwrap();
        self.get::<UserResponse>(&url).await.map(|res| res.user)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn build_test_client() -> Client {
        Client::new(
            "crates-io-api-continuous-integration (github.com/theduke/crates-io-api)",
            std::time::Duration::from_millis(1000),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn test_summary_async() -> Result<(), Error> {
        let client = build_test_client();
        let summary = client.summary().await?;
        assert!(summary.most_downloaded.len() > 0);
        assert!(summary.just_updated.len() > 0);
        assert!(summary.new_crates.len() > 0);
        assert!(summary.most_recently_downloaded.len() > 0);
        assert!(summary.num_crates > 0);
        assert!(summary.num_downloads > 0);
        assert!(summary.popular_categories.len() > 0);
        assert!(summary.popular_keywords.len() > 0);
        Ok(())
    }

    #[tokio::test]
    async fn test_crates_stream_async() {
        let client = build_test_client();

        let mut stream = client.crates_stream(CratesQuery {
            per_page: 10,
            ..Default::default()
        });

        for _ in 0..40 {
            let _krate = stream.next().await.unwrap().unwrap();
            eprintln!("CRATE {}", _krate.name);
        }
    }

    #[tokio::test]
    async fn test_full_crate_async() -> Result<(), Error> {
        let client = build_test_client();
        client.full_crate("crates_io_api", false).await?;

        Ok(())
    }

    #[tokio::test]
    async fn test_user_get_async() -> Result<(), Error> {
        let client = build_test_client();
        let user = client.user("theduke").await?;
        assert_eq!(user.login, "theduke");
        Ok(())
    }

    #[tokio::test]
    async fn test_crates_filter_by_user_async() -> Result<(), Error> {
        let client = build_test_client();

        let user = client.user("theduke").await?;

        let res = client
            .crates(CratesQuery {
                user_id: Some(user.id),
                per_page: 20,
                ..Default::default()
            })
            .await?;

        assert!(!res.crates.is_empty());
        // Ensure all found have the searched user as owner.
        for krate in res.crates {
            let owners = client.crate_owners(&krate.name).await?;
            assert!(owners.iter().any(|o| o.id == user.id));
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_crates_filter_by_category_async() -> Result<(), Error> {
        let client = build_test_client();

        let category = "wasm".to_string();

        let res = client
            .crates(CratesQuery {
                category: Some(category.clone()),
                per_page: 3,
                ..Default::default()
            })
            .await?;

        assert!(!res.crates.is_empty());
        // Ensure all found crates have the given category.
        for list_crate in res.crates {
            let krate = client.get_crate(&list_crate.name).await?;
            assert!(krate.categories.iter().any(|c| c.id == category));
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_crate_reverse_dependency_count_async() -> Result<(), Error> {
        let client = build_test_client();
        let count = client
            .crate_reverse_dependency_count("crates_io_api")
            .await?;
        assert!(count > 0);

        Ok(())
    }
}
