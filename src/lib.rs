use std::{
	borrow::Cow,
	collections::{HashMap, HashSet},
};

use regex_lite::Regex;
use scraper::{Html, Selector};
use url::Position;
use worker::*;

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SiteDefinition {
	url: String,
	follow_links: Option<String>,
	max_depth: Option<u32>,
	searches: Vec<Search>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct Search {
	selector: String,
	attributes: Vec<String>,
}

async fn load_site(link: String) -> Result<(String, String)> {
	let fetch = Fetch::Url(Url::parse(&link)?);
	let mut res = fetch.send().await?;
	res.text().await.map(|r| (r, link))
}

fn resolve_selectors<'name, 'result>(
	parsed: &'result Html,
	selectors: &[(&'name str, Selector, &[String])],
	results: &mut HashMap<&'name str, HashMap<&str, HashSet<Cow<'result, str>>>>,
) {
	for (selector_name, selector, attributes) in selectors {
		for element in parsed.select(selector) {
			let selector_group = results.get_mut(selector_name).unwrap();

			for attribute in attributes.iter() {
				let attribute_set = selector_group.get_mut(attribute.as_str()).unwrap();
				match attribute.as_str() {
					"TextContent" => {
						let text = element.text().collect::<String>();
						attribute_set.insert(Cow::Owned(text));
					}
					attribute => {
						if let Some(value) = element.value().attr(attribute) {
							attribute_set.insert(Cow::Borrowed(value));
						}
					}
				}
			}
		}
	}
}

#[event(fetch)]
pub async fn main(req: Request, env: Env, _: worker::Context) -> Result<Response> {
	// Environment bindings like KV Stores, Durable Objects, Secrets, and Variables.
	Router::new()
		.get("/", |_, _| Response::ok(concat!(env!("CARGO_PKG_NAME"), " v", env!("CARGO_PKG_VERSION"))))
		.post_async("/scrape", |mut req, _| async move {
			// Get more helpful error messages written to the console in the case of a panic.
			console_error_panic_hook::set_once();

			let SiteDefinition {
				url,
				follow_links,
				max_depth,
				searches,
			} = req.json::<SiteDefinition>().await?;

			// Initialize results map
			let mut results = searches
				.iter()
				.map(|Search { selector, attributes }| (selector.as_str(), attributes.iter().map(|a| (a.as_str(), HashSet::new())).collect::<HashMap<_, _>>()))
				.collect();

			// Parse selectors
			let selectors = searches
				.iter()
				.map(|Search { selector, attributes }| (selector.as_str(), Selector::parse(&selector).unwrap(), attributes.as_slice()))
				.collect::<Vec<_>>();

			// Queue tasks
			let mut pending_sites = HashSet::new();
			pending_sites.insert(url);

			let links_regex = match follow_links.map(|s| Regex::new(&s)) {
				Some(res) => Some(res.map_err(|e| e.to_string())?),
				None => None,
			};
			let links_selector = Selector::parse("a[href]").unwrap();

			let mut _temp = HashSet::new();
			let mut visited = HashSet::<Cow<str>>::new();
			let mut current_depth = 0u32;
			let mut document_cache = vec![];

			// Process tasks
			loop {
				if pending_sites.is_empty() || current_depth > max_depth.unwrap_or(0) {
					break;
				}

				let queue = pending_sites.drain().map(|site| load_site(site));
				for site in futures::future::join_all(queue).await {
					let (site_data, site) = site?;
					let parsed = Html::parse_document(&site_data);
					let homepage = &Url::parse(&site).unwrap()[..Position::AfterPort];
					let homepage = Url::parse(homepage).unwrap();

					// Explore and Enqueue links
					let new_links = parsed
						.select(&links_selector)
						.filter_map(|element| element.value().attr("href"))
						.filter_map(|link| match link.get(..1) {
							Some(c) => match c {
								"/" => {
									let formatted = homepage.join(link).unwrap();
									Some(formatted.to_string())
								}
								"#" => None,
								_ => Some(link.to_owned()),
							},
							None => None,
						})
						.filter(|link| links_regex.as_ref().map_or(false, |w| w.is_match(link)))
						.filter(|link| !visited.contains(&Cow::Borrowed(link.as_str())));

					_temp.extend(new_links);

					// Cache parsed documents for later processing
					document_cache.push(parsed);
					visited.insert(Cow::Owned(site));
				}

				// drain temp into pending_sites
				pending_sites.extend(_temp.drain());
				current_depth += 1;
			}

			// Populate results
			document_cache.iter().for_each(|doc| resolve_selectors(doc, selectors.as_slice(), &mut results));
			Response::from_json(&results)
		})
		.run(req, env)
		.await
}
