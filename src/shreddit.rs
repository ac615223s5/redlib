//! Comment trees and search results from Reddit's server-rendered web frontend ("shreddit").
//!
//! Reddit's edge blocks the OAuth comments and search APIs for anonymous
//! clients, but reddit.com still serves logged-out visitors HTML fragments of
//! comment trees and search results. This module crawls those fragments and
//! converts them into the API's `Listing` JSON, so parsing and rendering stay
//! unchanged.

use crate::client::shreddit_fragment;
use cached::proc_macro::cached;
use chrono::DateTime;
use log::warn;
use scraper::{ElementRef, Html, Selector};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::sync::LazyLock;
use tokio::task::JoinSet;
use url::{form_urlencoded, Url};

/// How much of a comment tree is crawled beyond the first fragment.
#[derive(Clone, Copy)]
struct CrawlLimits {
	/// Stop expanding "more comments" placeholders once this many comments are loaded.
	max_comments: usize,
	/// Upper bound on placeholder expansions.
	max_expansions: usize,
	/// Number of placeholder expansions fetched concurrently.
	concurrency: usize,
}

const CRAWL_LIMITS: CrawlLimits = CrawlLimits {
	max_comments: 200,
	max_expansions: 30,
	concurrency: 10,
};

const MORE_COMMENTS_PATH_PREFIX: &str = "/svc/shreddit/more-comments/";

static COMMENT: LazyLock<Selector> = LazyLock::new(|| Selector::parse("shreddit-comment").unwrap());
static COMMENT_BODY: LazyLock<Selector> = LazyLock::new(|| Selector::parse(r#"[slot="comment"]"#).unwrap());
static COMMENT_META: LazyLock<Selector> = LazyLock::new(|| Selector::parse(r#"[slot="commentMeta"]"#).unwrap());
static SPAN: LazyLock<Selector> = LazyLock::new(|| Selector::parse("span").unwrap());
static TIMEAGO: LazyLock<Selector> = LazyLock::new(|| Selector::parse("faceplate-timeago[ts]").unwrap());
static MORE_COMMENTS: LazyLock<Selector> = LazyLock::new(|| Selector::parse("faceplate-partial[src]").unwrap());
static CURSOR: LazyLock<Selector> = LazyLock::new(|| Selector::parse(r#"input[name="cursor"]"#).unwrap());
static COUNT: LazyLock<Selector> = LazyLock::new(|| Selector::parse("faceplate-number[number]").unwrap());
static PLAYER: LazyLock<Selector> = LazyLock::new(|| Selector::parse("shreddit-player[src]").unwrap());

/// Fetch the comment tree of a post as a Reddit API comment `Listing`.
///
/// When `comment_id` is set, only that comment's thread is returned, preceded
/// by up to `context` ancestors, mirroring the API's comment permalinks.
#[cached(size = 100, time = 30, result = true)]
pub async fn comments(subreddit: String, post_id: String, comment_id: Option<String>, sort: String, context: Option<String>) -> Result<Value, String> {
	let html = shreddit_fragment(comments_path(&subreddit, &post_id, comment_id.as_deref(), &sort), None).await?;

	let mut tree = CommentTree::new(format!("t3_{post_id}"), comment_id.map(|id| format!("t1_{id}")));
	tree.add(parse_fragment(&html));
	expand(&mut tree, CRAWL_LIMITS, |path, cursor| shreddit_fragment(path, Some(cursor))).await;

	let context = context.and_then(|context| context.parse().ok()).unwrap_or(0);
	Ok(tree.into_listing(context))
}

fn comments_path(subreddit: &str, post_id: &str, comment_id: Option<&str>, sort: &str) -> String {
	let mut path = format!("/svc/shreddit/comments/r/{subreddit}/t3_{post_id}");
	if let Some(comment_id) = comment_id {
		path.push_str(&format!("/t1_{comment_id}"));
	}
	if !sort.is_empty() {
		path.push_str(&format!("?{}", url::form_urlencoded::Serializer::new(String::new()).append_pair("sort", sort).finish()));
	}
	path
}

/// Expand "more comments" placeholders breadth-first, keeping up to
/// `limits.concurrency` requests in flight. Placeholders that fail or exceed
/// `limits` stay in the tree as stubs.
async fn expand<F, Fut>(tree: &mut CommentTree, limits: CrawlLimits, fetch: F)
where
	F: Fn(String, String) -> Fut,
	Fut: Future<Output = Result<String, String>> + Send + 'static,
{
	let mut in_flight = JoinSet::new();
	let mut expansions = 0;
	loop {
		while in_flight.len() < limits.concurrency && expansions < limits.max_expansions && tree.len() < limits.max_comments {
			let Some(more) = tree.next_wanted() else { break };
			let request = fetch(more.path.clone(), more.cursor.clone());
			in_flight.spawn(async move { (more, request.await) });
			expansions += 1;
		}

		match in_flight.join_next().await {
			None => break,
			Some(Ok((_, Ok(html)))) => tree.add(parse_fragment(&html)),
			Some(Ok((more, Err(e)))) => {
				warn!("Couldn't expand more comments: {e}");
				tree.skip(more);
			}
			Some(Err(e)) => warn!("Couldn't expand more comments: {e}"),
		}
	}
}

/// A comment parsed from a fragment, before its parent is resolved.
struct ParsedComment {
	id: String,
	parent_id: Option<String>,
	depth: u32,
	position: String,
	parent_positions: String,
	data: Value,
}

/// A "more comments" placeholder that can be expanded with a cursor.
struct MoreComments {
	path: String,
	cursor: String,
	count: i64,
	/// Position path of the parent comment; `None` for more top-level comments.
	parent_positions: Option<String>,
}

struct Fragment {
	comments: Vec<ParsedComment>,
	more: Vec<MoreComments>,
}

enum Child {
	Comment(String),
	More(i64),
}

/// Comments accumulated across fragments, keyed by fullname (`t1_...`).
struct CommentTree {
	post: String,
	focus: Option<String>,
	data: HashMap<String, Value>,
	parents: HashMap<String, String>,
	children: HashMap<String, Vec<Child>>,
	/// Shreddit addresses comments by their path of sibling positions, e.g. `0,2`.
	positions: HashMap<String, String>,
	pending: VecDeque<MoreComments>,
	skipped: Vec<MoreComments>,
}

impl CommentTree {
	fn new(post: String, focus: Option<String>) -> Self {
		Self {
			post,
			focus,
			data: HashMap::new(),
			parents: HashMap::new(),
			children: HashMap::new(),
			positions: HashMap::new(),
			pending: VecDeque::new(),
			skipped: Vec::new(),
		}
	}

	fn len(&self) -> usize {
		self.data.len()
	}

	fn add(&mut self, fragment: Fragment) {
		for comment in fragment.comments {
			if self.data.contains_key(&comment.id) {
				continue;
			}
			let path = if comment.parent_positions.is_empty() {
				comment.position.clone()
			} else {
				format!("{},{}", comment.parent_positions, comment.position)
			};
			let parent = comment
				.parent_id
				.or_else(|| (comment.depth > 0).then(|| self.positions.get(&comment.parent_positions).cloned()).flatten())
				.unwrap_or_else(|| self.post.clone());

			let mut data = comment.data;
			data["parent_id"] = Value::String(parent.clone());

			self.positions.insert(path, comment.id.clone());
			self.children.entry(parent.clone()).or_default().push(Child::Comment(comment.id.clone()));
			self.parents.insert(comment.id.clone(), parent);
			self.data.insert(comment.id, data);
		}
		self.pending.extend(fragment.more);
	}

	/// Take the next placeholder worth expanding, breadth-first.
	fn next_wanted(&mut self) -> Option<MoreComments> {
		while let Some(more) = self.pending.pop_front() {
			if self.wants(&more) {
				return Some(more);
			}
			self.skipped.push(more);
		}
		None
	}

	fn skip(&mut self, more: MoreComments) {
		self.skipped.push(more);
	}

	/// In single-thread mode, only expand placeholders inside the focused thread.
	fn wants(&self, more: &MoreComments) -> bool {
		let Some(focus) = &self.focus else { return true };
		self.lineage(&self.placeholder_parent(more)).any(|id| id == focus)
	}

	/// `id` followed by its ancestor comments, nearest first.
	fn lineage<'a>(&'a self, id: &'a String) -> impl Iterator<Item = &'a String> {
		std::iter::successors(Some(id), |id| self.parents.get(*id))
			.take_while(|id| **id != self.post)
			// Malformed `parentId` attributes could form a cycle.
			.take(self.data.len() + 1)
	}

	fn placeholder_parent(&self, more: &MoreComments) -> String {
		more
			.parent_positions
			.as_ref()
			.and_then(|positions| self.positions.get(positions))
			.cloned()
			.unwrap_or_else(|| self.post.clone())
	}

	fn into_listing(mut self, context: usize) -> Value {
		let unexpanded: Vec<MoreComments> = self.pending.drain(..).chain(self.skipped.drain(..)).collect();
		for more in unexpanded {
			let parent = self.placeholder_parent(&more);
			self.children.entry(parent).or_default().push(Child::More(more.count));
		}

		let thread = self.focus.as_ref().filter(|focus| self.data.contains_key(*focus)).map(|focus| {
			let mut thread: Vec<String> = self.lineage(focus).take(context + 1).cloned().collect();
			thread.reverse();
			thread
		});

		match thread.as_deref() {
			Some([root, rest @ ..]) => listing(vec![self.child(&self.parents[root], &Child::Comment(root.clone()), rest)]),
			_ => self.listing(&self.post, &[]),
		}
	}

	/// Build the `Listing` of `parent`'s replies. While `thread` is non-empty,
	/// only its next comment is kept at each level.
	fn listing(&self, parent: &str, thread: &[String]) -> Value {
		let children = self.children.get(parent).map(Vec::as_slice).unwrap_or_default();
		listing(match thread.split_first() {
			Some((next, rest)) => children
				.iter()
				.filter(|child| matches!(child, Child::Comment(id) if id == next))
				.map(|child| self.child(parent, child, rest))
				.collect(),
			None => children.iter().map(|child| self.child(parent, child, &[])).collect(),
		})
	}

	fn child(&self, parent: &str, child: &Child, thread: &[String]) -> Value {
		match child {
			Child::Comment(id) => {
				let mut data = self.data[id].clone();
				let has_replies = self.children.get(id).is_some_and(|replies| !replies.is_empty());
				data["replies"] = if has_replies { self.listing(id, thread) } else { Value::String(String::new()) };
				json!({ "kind": "t1", "data": data })
			}
			Child::More(count) => json!({
				"kind": "more",
				"data": { "count": count, "parent_id": parent, "id": "_", "children": [] }
			}),
		}
	}
}

fn listing(children: Vec<Value>) -> Value {
	json!({ "kind": "Listing", "data": { "children": children } })
}

fn parse_fragment(html: &str) -> Fragment {
	let document = Html::parse_document(html);
	Fragment {
		comments: document.select(&COMMENT).filter_map(parse_comment).collect(),
		more: document.select(&MORE_COMMENTS).filter_map(parse_more_comments).collect(),
	}
}

fn parse_comment(element: ElementRef<'_>) -> Option<ParsedComment> {
	let attr = |name: &str| element.value().attr(name);
	let id = attr("thingid")?.to_string();
	let author = attr("author").unwrap_or("[deleted]");
	let is_moderator = attr("is-mod-distinguished").is_some();

	let content = comment_content(element, &id);
	let body_html = content.map(native_media_html).unwrap_or_default();
	let body_text = content.map(|content| content.text().collect::<String>().trim().to_string()).unwrap_or_default();

	Some(ParsedComment {
		parent_id: attr("parentid").map(str::to_string),
		depth: attr("depth").and_then(|depth| depth.parse().ok()).unwrap_or(0),
		position: attr("comment-position").unwrap_or_default().to_string(),
		parent_positions: attr("comment-parent-positions").unwrap_or_default().trim_matches(['[', ']']).to_string(),
		data: json!({
			"id": id.trim_start_matches("t1_"),
			"name": id,
			"author": author,
			"body": body_text,
			"body_html": format!("<div class=\"md\">{body_html}</div>"),
			"score": attr("score").and_then(|score| score.parse::<i64>().ok()).unwrap_or(0),
			"created_utc": attr("created").and_then(unix_time).unwrap_or_default(),
			"edited": edited_time(element).map_or(Value::Bool(false), Value::from),
			"distinguished": if is_moderator { Value::from("moderator") } else { Value::Null },
			// Shreddit only marks stickied mod comments by collapsing them.
			"stickied": is_moderator && attr("collapsed").is_some(),
			"permalink": attr("permalink").unwrap_or_default(),
		}),
		id,
	})
}

/// The element holding a comment's rendered body, if it has one.
fn comment_content<'a>(element: ElementRef<'a>, id: &str) -> Option<ElementRef<'a>> {
	// A comment without a body would otherwise match the first reply's body.
	let body_id = format!("{id}-comment-rtjson-content");
	let body = element.select(&COMMENT_BODY).next().filter(|body| body.value().id() == Some(body_id.as_str()))?;
	Some(
		body
			.child_elements()
			.find(|child| child.value().id().is_some_and(|child_id| child_id.ends_with("-post-rtjson-content")))
			.unwrap_or(body),
	)
}

/// The inner HTML of `content`, with Reddit's script-driven media players
/// replaced by native videos so they render without Reddit's JavaScript.
fn native_media_html(content: ElementRef<'_>) -> String {
	content
		.select(&PLAYER)
		.fold(content.inner_html(), |html, player| html.replacen(&player.html(), &native_video(player), 1))
}

fn native_video(player: ElementRef<'_>) -> String {
	let src = player.value().attr("src").unwrap_or_default().replace('&', "&amp;").replace('"', "&quot;");
	// Reddit serves comment GIFs as silent MP4s that it autoplays in a loop.
	let playback = if player.value().attr("gif").is_some() {
		"autoplay loop muted playsinline"
	} else {
		"controls preload=\"metadata\""
	};
	format!(r#"<video src="{src}" {playback}></video>"#)
}

fn edited_time(element: ElementRef<'_>) -> Option<f64> {
	element
		.select(&COMMENT_META)
		.next()?
		.select(&SPAN)
		.find(|span| span.text().collect::<String>().trim_start().starts_with("Edited"))?
		.select(&TIMEAGO)
		.next()
		.and_then(|timeago| unix_time(timeago.value().attr("ts")?))
}

fn parse_more_comments(element: ElementRef<'_>) -> Option<MoreComments> {
	let path = element.value().attr("src")?;
	if !path.starts_with(MORE_COMMENTS_PATH_PREFIX) {
		return None;
	}
	let cursor = element.select(&CURSOR).next()?.value().attr("value")?;
	let query: HashMap<String, String> = path
		.split_once('?')
		.map(|(_, query)| url::form_urlencoded::parse(query.as_bytes()).into_owned().collect())
		.unwrap_or_default();

	let count = element
		.select(&COUNT)
		.next()
		.and_then(|count| count.value().attr("number"))
		.or_else(|| query.get("comments-remaining").map(String::as_str))
		.and_then(|count| count.parse().ok())
		.unwrap_or(0);

	Some(MoreComments {
		path: path.to_string(),
		cursor: cursor.to_string(),
		count,
		parent_positions: query.get("commentParentPositions").cloned(),
	})
}

/// Parse shreddit's RFC 3339 timestamps (`...Z` or `...+0000`) into Unix seconds.
fn unix_time(timestamp: &str) -> Option<f64> {
	DateTime::parse_from_rfc3339(timestamp)
		.or_else(|_| DateTime::parse_from_str(timestamp, "%Y-%m-%dT%H:%M:%S%.f%z"))
		.ok()
		.map(|time| time.timestamp_millis() as f64 / 1000.0)
}

static HOVER_CARD: LazyLock<Selector> = LazyLock::new(|| Selector::parse(r#"[data-testid="user-hover-card"]"#).unwrap());
static NAMEPLATE: LazyLock<Selector> = LazyLock::new(|| Selector::parse(r#"[data-testid="nameplate"]"#).unwrap());
static AVATAR: LazyLock<Selector> = LazyLock::new(|| Selector::parse("img[src]").unwrap());
static CAKE_DAY: LazyLock<Selector> = LazyLock::new(|| Selector::parse(r#"time[data-testid="cake-day"][datetime]"#).unwrap());
static KARMA: LazyLock<Selector> = LazyLock::new(|| Selector::parse(r#"[data-testid="karma-number"]"#).unwrap());

/// Profile details shown on a user's hover card. The `about.json` endpoint is
/// blocked for anonymous clients, so this is the only source of karma and
/// account age.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HoverCard {
	pub name: String,
	pub avatar: String,
	/// Unix seconds of the account's cake day.
	pub created: f64,
	/// Post karma plus comment karma.
	pub karma: i64,
}

#[cached(size = 100, time = 600, result = true)]
pub async fn user_hover_card(name: String) -> Result<HoverCard, String> {
	if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
		return Err(format!("Invalid username | {name}"));
	}
	let html = shreddit_fragment(format!("/svc/shreddit/user-hover-card/{name}"), None).await?;
	parse_hover_card(&html).ok_or_else(|| format!("User not found | {name}"))
}

/// Returns `None` when Reddit renders its error alert instead of a card,
/// which happens for deleted, suspended and nonexistent accounts.
fn parse_hover_card(html: &str) -> Option<HoverCard> {
	let document = Html::parse_fragment(html);
	let card = document.select(&HOVER_CARD).next()?;
	let text = |element: ElementRef| element.text().collect::<String>().trim().to_string();

	Some(HoverCard {
		name: card.select(&NAMEPLATE).next().map(text).unwrap_or_default(),
		avatar: card.select(&AVATAR).next().and_then(|img| img.attr("src")).unwrap_or_default().to_string(),
		created: card.select(&CAKE_DAY).next().and_then(|time| time.attr("datetime")).and_then(unix_time).unwrap_or_default(),
		karma: card.select(&KARMA).filter_map(|karma| text(karma).replace(',', "").parse::<i64>().ok()).sum(),
	})
}

// SEARCH

/// Result fragments fetched per search page. Reddit puts 7 posts in each.
const SEARCH_FRAGMENTS_PER_PAGE: usize = 3;

/// One post card; its telemetry context carries the post's metadata as JSON.
static SEARCH_POST: LazyLock<Selector> = LazyLock::new(|| Selector::parse(r#"search-telemetry-tracker[view-events="search/view/post"][data-faceplate-tracking-context]"#).unwrap());
static SEARCH_TITLE: LazyLock<Selector> = LazyLock::new(|| Selector::parse(r#"a[data-testid="post-title"][href]"#).unwrap());
static SEARCH_COUNTER: LazyLock<Selector> = LazyLock::new(|| Selector::parse(r#"[data-testid="search-counter-row"] > span"#).unwrap());
static SEARCH_THUMBNAIL: LazyLock<Selector> = LazyLock::new(|| Selector::parse(r#"faceplate-img[data-testid="search_post_thumbnail"][src]"#).unwrap());
/// The lazily loaded next page of results.
static SEARCH_NEXT: LazyLock<Selector> = LazyLock::new(|| Selector::parse(r#"faceplate-partial[loading="lazy"][src*="cursor="]"#).unwrap());

/// Search posts, site-wide or within `subreddit` (`a+b` searches several), as
/// a Reddit API `Listing`. Its `after` is Reddit's cursor for the next page.
///
/// Logged-out search is Reddit's safe search: NSFW posts are never returned.
#[cached(size = 100, time = 60, result = true)]
pub async fn search(subreddit: Option<String>, query: String, sort: String, time: String, cursor: Option<String>) -> Result<Value, String> {
	let mut posts = Vec::new();
	let mut seen = HashSet::new();
	let mut cursor = cursor;
	for _ in 0..SEARCH_FRAGMENTS_PER_PAGE {
		let path = search_path(subreddit.as_deref(), &query, &sort, &time, cursor.as_deref())?;
		let page = parse_search_fragment(&shreddit_fragment(path, None).await?);
		posts.extend(page.posts.into_iter().filter(|post| seen.insert(post["data"]["id"].as_str().unwrap_or_default().to_string())));
		cursor = page.cursor;
		if cursor.is_none() {
			break;
		}
	}

	Ok(json!({ "kind": "Listing", "data": { "after": cursor.unwrap_or_default(), "children": posts } }))
}

fn search_path(subreddit: Option<&str>, query: &str, sort: &str, time: &str, cursor: Option<&str>) -> Result<String, String> {
	let subreddits: Vec<&str> = subreddit.map(|sub| sub.split('+').filter(|name| !name.is_empty()).collect()).unwrap_or_default();
	if !subreddits.iter().all(|name| name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')) {
		return Err(format!("Invalid subreddit name | {}", subreddit.unwrap_or_default()));
	}

	// Subreddit search pages only cover one subreddit, so several are searched
	// site-wide with Reddit's subreddit: operator.
	let (base, query) = match subreddits.as_slice() {
		[] => ("/svc/shreddit/search/".to_string(), query.to_string()),
		[name] => (format!("/svc/shreddit/r/{name}/search/"), query.to_string()),
		names => (
			"/svc/shreddit/search/".to_string(),
			format!("{query} ({})", names.iter().map(|name| format!("subreddit:{name}")).collect::<Vec<_>>().join(" OR ")),
		),
	};

	let mut params = form_urlencoded::Serializer::new(String::new());
	params.append_pair("q", &query);
	for (name, value) in [("sort", sort), ("t", time), ("cursor", cursor.unwrap_or_default())] {
		if !value.is_empty() {
			params.append_pair(name, value);
		}
	}
	Ok(format!("{base}?{}", params.finish()))
}

struct SearchFragment {
	posts: Vec<Value>,
	cursor: Option<String>,
}

fn parse_search_fragment(html: &str) -> SearchFragment {
	let document = Html::parse_document(html);
	SearchFragment {
		posts: document.select(&SEARCH_POST).filter_map(parse_search_post).collect(),
		cursor: document.select(&SEARCH_NEXT).find_map(|element| query_param(element.value().attr("src")?, "cursor")),
	}
}

fn parse_search_post(element: ElementRef<'_>) -> Option<Value> {
	let context: Value = serde_json::from_str(element.value().attr("data-faceplate-tracking-context")?).ok()?;
	let post = &context["post"];
	let id = post["id"].as_str()?.strip_prefix("t3_")?;
	let title_link = element.select(&SEARCH_TITLE).next()?;
	let permalink = title_link.value().attr("href")?;
	let title = post["title"].as_str().or_else(|| title_link.value().attr("aria-label")).unwrap_or_default();
	let subreddit = context["subreddit"]["name"]
		.as_str()
		.or_else(|| permalink.strip_prefix("/r/")?.split('/').next())
		.unwrap_or_default();

	let created = element
		.select(&TIMEAGO)
		.next()
		.and_then(|time| unix_time(time.value().attr("ts")?))
		.unwrap_or_default();

	let (mut score, mut comments) = (None, 0);
	for counter in element.select(&SEARCH_COUNTER) {
		let Some(number) = counter.select(&COUNT).next().and_then(|count| count.value().attr("number")?.parse::<i64>().ok()) else {
			continue;
		};
		if counter.text().any(|text| text.contains("comment")) {
			comments = number;
		} else {
			score = Some(number);
		}
	}

	let thumbnail = element.select(&SEARCH_THUMBNAIL).next().and_then(|image| image.value().attr("src")).unwrap_or_default();
	let thumbnail_size = |name: &str| query_param(thumbnail, name).and_then(|size| size.parse::<i64>().ok()).unwrap_or(140);

	Some(json!({
		"kind": "t3",
		"data": {
			"id": id,
			"name": format!("t3_{id}"),
			"title": title,
			"subreddit": subreddit,
			"author": context["profile"]["name"].as_str().unwrap_or("[deleted]"),
			"permalink": permalink,
			"url": permalink,
			// Cards don't say what a post links to, so posts with a thumbnail are
			// shown as links to their comments and the rest as text posts.
			// No selftext: the snippet on site-wide cards is often a matching
			// comment rather than the post's text.
			"is_self": thumbnail.is_empty(),
			"thumbnail": thumbnail,
			"thumbnail_width": thumbnail_size("width"),
			"thumbnail_height": thumbnail_size("height"),
			"score": score.unwrap_or_default(),
			"hide_score": score.is_none(),
			"num_comments": comments,
			"created_utc": created,
			"over_18": post["nsfw"].as_bool().unwrap_or_default() || context["subreddit"]["nsfw"].as_bool().unwrap_or_default(),
			"spoiler": post["spoiler"].as_bool().unwrap_or_default(),
		}
	}))
}

/// The value of query parameter `name` in a URL or root-relative path.
fn query_param(url: &str, name: &str) -> Option<String> {
	let url = Url::parse(url).or_else(|_| Url::parse("https://www.reddit.com")?.join(url)).ok()?;
	url.query_pairs().find(|(key, _)| key == name).map(|(_, value)| value.into_owned())
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::sync::{Arc, Mutex};

	const FRAGMENT: &str = r#"
<shreddit-comment thingId="t1_a" author="alice" depth="0" comment-position="0" comment-parent-positions="[]" score="12" created="2026-10-22T15:44:22.205000+0000" permalink="/r/test/comments/p/comment/a/">
  <details><summary><div slot="commentMeta"><span>alice</span><span>Edited <faceplate-timeago ts="2026-10-22T19:43:36.170Z"></faceplate-timeago></span></div></summary>
  <div id="t1_a-comment-rtjson-content" slot="comment"><div id="t1_a-post-rtjson-content"><p>Hello <a href="https://www.reddit.com/r/rust">r/rust</a></p></div></div>
  <shreddit-comment thingId="t1_b" author="bob" depth="1" parentId="t1_a" comment-position="0" comment-parent-positions="[0]" score="3" created="2026-10-22T16:00:00.000000+0000" is-mod-distinguished collapsed>
    <div id="t1_b-comment-rtjson-content" slot="comment"><div id="t1_b-post-rtjson-content"><p>Reply</p></div></div>
    <faceplate-partial src="/svc/shreddit/more-comments/test/t3_p?startingDepth=2&amp;commentParentPositions=0,0" method="post">
      <input type="hidden" name="cursor" value="c1"><faceplate-number number="7"></faceplate-number> more replies
    </faceplate-partial>
  </shreddit-comment>
  <faceplate-partial src="/svc/shreddit/more-comments/test/t3_p?startingDepth=1&amp;commentPositionOffset=1&amp;commentParentPositions=0" method="post">
    <input type="hidden" name="cursor" value="c2"><faceplate-number number="4"></faceplate-number> more replies
  </faceplate-partial>
  </details>
</shreddit-comment>
<shreddit-comment thingId="t1_c" depth="0" comment-position="1" comment-parent-positions="[]" score="1"></shreddit-comment>
<faceplate-partial src="/svc/shreddit/more-comments/test/t3_p?top-level=1&amp;comments-remaining=40&amp;commentPositionOffset=2" method="post">
  <input type="hidden" name="cursor" value="c3">
</faceplate-partial>
<faceplate-partial src="/svc/shreddit/user-hover-card/alice"></faceplate-partial>
"#;

	const MORE_REPLIES: &str = r#"
<shreddit-comment thingId="t1_d" author="dave" depth="1" comment-position="1" comment-parent-positions="[0]" score="2"></shreddit-comment>
<shreddit-comment thingId="t1_e" author="erin" depth="2" comment-position="0" comment-parent-positions="[0,1]" score="2"></shreddit-comment>
"#;

	fn tree(focus: Option<&str>) -> CommentTree {
		let mut tree = CommentTree::new("t3_p".into(), focus.map(str::to_string));
		tree.add(parse_fragment(FRAGMENT));
		tree
	}

	#[test]
	fn test_parse_fragment() {
		let fragment = parse_fragment(FRAGMENT);
		let ids: Vec<&str> = fragment.comments.iter().map(|comment| comment.id.as_str()).collect();
		assert_eq!(ids, ["t1_a", "t1_b", "t1_c"]);

		let alice = &fragment.comments[0].data;
		assert_eq!(alice["id"], "a");
		assert_eq!(alice["body"], "Hello r/rust");
		assert!(alice["body_html"]
			.as_str()
			.unwrap()
			.starts_with(r#"<div class="md"><p>Hello <a href="https://www.reddit.com/r/rust">"#));
		assert_eq!(alice["score"], 12);
		assert_eq!(alice["created_utc"], 1_792_683_862.205);
		assert_eq!(alice["edited"], 1_792_698_216.17);
		assert_eq!(alice["distinguished"], Value::Null);

		let bob = &fragment.comments[1].data;
		assert_eq!(bob["edited"], false);
		assert_eq!(bob["distinguished"], "moderator");
		assert_eq!(bob["stickied"], true);
		assert_eq!(fragment.comments[2].data["author"], "[deleted]");
		assert_eq!(fragment.comments[2].data["body"], "");

		let more: Vec<(&str, i64, Option<&str>)> = fragment
			.more
			.iter()
			.map(|more| (more.cursor.as_str(), more.count, more.parent_positions.as_deref()))
			.collect();
		assert_eq!(more, [("c1", 7, Some("0,0")), ("c2", 4, Some("0")), ("c3", 40, None)]);
	}

	#[test]
	fn test_listing_builds_tree_and_more_stubs() {
		let mut tree = tree(None);
		let more = tree.next_wanted().unwrap();
		assert_eq!(more.cursor, "c1");
		tree.skip(more);
		tree.add(parse_fragment(MORE_REPLIES));

		let listing = tree.into_listing(0);
		let top = &listing["data"]["children"];
		assert_eq!(top.as_array().unwrap().len(), 3);
		assert_eq!(top[0]["data"]["id"], "a");
		assert_eq!(top[0]["data"]["parent_id"], "t3_p");
		assert_eq!(top[1]["data"]["id"], "c");
		assert_eq!(top[1]["data"]["replies"], "");
		assert_eq!(top[2]["kind"], "more");
		assert_eq!(top[2]["data"]["count"], 40);

		let replies = &top[0]["data"]["replies"]["data"]["children"];
		let reply_ids: Vec<&str> = replies.as_array().unwrap().iter().map(|reply| reply["data"]["id"].as_str().unwrap()).collect();
		assert_eq!(reply_ids, ["b", "d", "_"]);
		assert_eq!(replies[1]["data"]["parent_id"], "t1_a");
		assert_eq!(replies[2]["data"]["parent_id"], "t1_a");
		assert_eq!(replies[1]["data"]["replies"]["data"]["children"][0]["data"]["parent_id"], "t1_d");

		let bob_replies = &replies[0]["data"]["replies"]["data"]["children"];
		assert_eq!(bob_replies[0]["kind"], "more");
		assert_eq!(bob_replies[0]["data"]["parent_id"], "t1_b");
		assert_eq!(bob_replies[0]["data"]["count"], 7);
	}

	#[test]
	fn test_focused_thread() {
		let mut tree = tree(Some("t1_b"));
		assert_eq!(wanted_cursors(&mut tree), ["c1"]);
		let focused = tree.into_listing(0);
		assert_eq!(focused["data"]["children"].as_array().unwrap().len(), 1);
		assert_eq!(focused["data"]["children"][0]["data"]["id"], "b");

		let with_context = self::tree(Some("t1_b")).into_listing(3);
		let top = &with_context["data"]["children"];
		assert_eq!(top.as_array().unwrap().len(), 1);
		assert_eq!(top[0]["data"]["id"], "a");
		let replies = &top[0]["data"]["replies"]["data"]["children"];
		assert_eq!(replies.as_array().unwrap().len(), 1);
		assert_eq!(replies[0]["data"]["id"], "b");
	}

	fn ids(listing: &Value) -> Vec<String> {
		listing["data"]["children"]
			.as_array()
			.unwrap()
			.iter()
			.map(|child| child["data"]["id"].as_str().unwrap().to_string())
			.collect()
	}

	fn wanted_cursors(tree: &mut CommentTree) -> Vec<String> {
		std::iter::from_fn(|| tree.next_wanted()).map(|more| more.cursor).collect()
	}

	fn limits(max_comments: usize, max_expansions: usize, concurrency: usize) -> CrawlLimits {
		CrawlLimits {
			max_comments,
			max_expansions,
			concurrency,
		}
	}

	/// A fetcher that records requested cursors and answers `c2` with `MORE_REPLIES`.
	fn fetcher(calls: Arc<Mutex<Vec<String>>>) -> impl Fn(String, String) -> std::future::Ready<Result<String, String>> {
		move |_path, cursor| {
			calls.lock().unwrap().push(cursor.clone());
			std::future::ready(match cursor.as_str() {
				"c2" => Ok(MORE_REPLIES.to_string()),
				_ => Err(format!("no fragment for {cursor}")),
			})
		}
	}

	#[test]
	fn test_comments_path() {
		assert_eq!(comments_path("rust", "abc", None, ""), "/svc/shreddit/comments/r/rust/t3_abc");
		assert_eq!(comments_path("rust", "abc", None, "new"), "/svc/shreddit/comments/r/rust/t3_abc?sort=new");
		assert_eq!(comments_path("rust", "abc", Some("def"), "q&a"), "/svc/shreddit/comments/r/rust/t3_abc/t1_def?sort=q%26a");
	}

	#[test]
	fn test_unix_time() {
		assert_eq!(unix_time("1970-01-01T00:00:01.500Z"), Some(1.5));
		assert_eq!(unix_time("1970-01-01T00:00:01.500000+0000"), Some(1.5));
		assert_eq!(unix_time("1970-01-01T01:00:00+01:00"), Some(0.0));
		assert_eq!(unix_time("yesterday"), None);
		assert_eq!(unix_time(""), None);
	}

	#[test]
	fn test_parse_comment_edge_cases() {
		let fragment = parse_fragment(
			r#"
<shreddit-comment author="nobody" depth="0"></shreddit-comment>
<shreddit-comment thingId="t1_x" author="xavier" score="many" collapsed>
  <shreddit-comment thingId="t1_y" author="yara" is-mod-distinguished>
    <div id="t1_y-comment-rtjson-content" slot="comment"><p>Outer only</p></div>
  </shreddit-comment>
</shreddit-comment>"#,
		);
		let ids: Vec<&str> = fragment.comments.iter().map(|comment| comment.id.as_str()).collect();
		assert_eq!(ids, ["t1_x", "t1_y"], "comments without an ID are skipped");

		let xavier = &fragment.comments[0];
		assert_eq!(xavier.data["body"], "", "a missing body must not borrow the reply's body");
		assert_eq!(xavier.data["body_html"], r#"<div class="md"></div>"#);
		assert_eq!(xavier.data["score"], 0);
		assert_eq!(xavier.data["created_utc"], 0.0);
		assert_eq!(xavier.data["stickied"], false, "only moderator comments are stickied");
		assert_eq!(xavier.depth, 0);
		assert_eq!(xavier.position, "");
		assert_eq!(xavier.parent_positions, "");

		let yara = &fragment.comments[1];
		assert_eq!(yara.data["body"], "Outer only");
		assert_eq!(yara.data["body_html"], r#"<div class="md"><p>Outer only</p></div>"#);
		assert_eq!(yara.data["distinguished"], "moderator");
		assert_eq!(yara.data["stickied"], false, "uncollapsed moderator comments are not stickied");
	}

	#[test]
	fn test_players_become_native_videos() {
		let fragment = parse_fragment(
			r#"
<shreddit-comment thingId="t1_m">
  <div id="t1_m-comment-rtjson-content" slot="comment"><shreddit-async-loader bundlename="shreddit_player_loader">
    <figure class="rte-media"><a href="/preview/external-pre/g.gif?auto=webp&amp;s=1"><div style="width: 200px; height: 200px;">
      <shreddit-player autoplay="" gif="" src="/preview/external-pre/g.gif?width=200&amp;format=mp4&amp;s=2"><source src="/preview/external-pre/g.gif?width=200&amp;format=mp4&amp;s=2"></shreddit-player>
    </div></a></figure>
    <shreddit-player src="/preview/pre/v.mp4?q=&quot;x&quot;"></shreddit-player>
  </shreddit-async-loader></div>
</shreddit-comment>"#,
		);
		let html = fragment.comments[0].data["body_html"].as_str().unwrap();
		assert!(!html.contains("<shreddit-player"), "{html}");
		assert!(
			html.contains(r#"<a href="/preview/external-pre/g.gif?auto=webp&amp;s=1"><div style="width: 200px; height: 200px;">"#),
			"the link and sizing around a player are kept: {html}"
		);
		assert!(
			html.contains(r#"<video src="/preview/external-pre/g.gif?width=200&amp;format=mp4&amp;s=2" autoplay loop muted playsinline></video>"#),
			"{html}"
		);
		assert!(
			html.contains(r#"<video src="/preview/pre/v.mp4?q=&quot;x&quot;" controls preload="metadata"></video>"#),
			"{html}"
		);
	}

	#[test]
	fn test_parse_more_comments_edge_cases() {
		let fragment = parse_fragment(
			r#"
<faceplate-partial src="/svc/shreddit/more-comments/t/t3_p?commentParentPositions=1"></faceplate-partial>
<faceplate-partial src="https://evil.example/svc/shreddit/more-comments/t/t3_p"><input name="cursor" value="evil"></faceplate-partial>
<faceplate-partial src="/svc/shreddit/more-comments/t/t3_p?commentParentPositions=1"><input name="cursor" value="no-count"></faceplate-partial>
<faceplate-partial src="/svc/shreddit/more-comments/t/t3_p"><input name="cursor" value="c+d"><faceplate-number number="x"></faceplate-number></faceplate-partial>"#,
		);
		let more: Vec<(&str, i64, Option<&str>)> = fragment
			.more
			.iter()
			.map(|more| (more.cursor.as_str(), more.count, more.parent_positions.as_deref()))
			.collect();
		assert_eq!(more, [("no-count", 0, Some("1")), ("c+d", 0, None)]);
		assert_eq!(fragment.more[0].path, "/svc/shreddit/more-comments/t/t3_p?commentParentPositions=1");
	}

	#[test]
	fn test_add_deduplicates_and_resolves_parents() {
		let mut tree = tree(None);
		tree.add(parse_fragment(FRAGMENT));
		assert_eq!(tree.len(), 3, "comments seen in several fragments are kept once");
		assert_eq!(tree.pending.len(), 6, "placeholders are not deduplicated");

		tree.add(parse_fragment(
			r#"<shreddit-comment thingId="t1_orphan" depth="3" comment-position="0" comment-parent-positions="[9,9]"></shreddit-comment>"#,
		));
		assert_eq!(tree.parents["t1_b"], "t1_a", "parentId attribute");
		assert_eq!(tree.parents["t1_c"], "t3_p", "top-level comment");
		assert_eq!(tree.parents["t1_orphan"], "t3_p", "unknown positions fall back to the post");

		tree.add(parse_fragment(MORE_REPLIES));
		assert_eq!(tree.parents["t1_d"], "t1_a", "resolved through comment positions");
		assert_eq!(tree.parents["t1_e"], "t1_d", "resolved through nested comment positions");
	}

	#[test]
	fn test_next_wanted_is_breadth_first() {
		let mut tree = tree(None);
		assert_eq!(wanted_cursors(&mut tree), ["c1", "c2", "c3"]);
		assert!(tree.next_wanted().is_none());
	}

	#[test]
	fn test_focus_only_expands_descendants() {
		let mut tree = tree(Some("t1_a"));
		assert_eq!(wanted_cursors(&mut tree), ["c1", "c2"], "the top-level placeholder is outside the thread");
		assert_eq!(tree.skipped.len(), 1);
	}

	#[test]
	fn test_focus_missing_returns_full_tree() {
		let listing = tree(Some("t1_missing")).into_listing(0);
		assert_eq!(ids(&listing), ["a", "c", "_"]);
	}

	#[test]
	fn test_context_stops_at_top_level() {
		let mut tree = tree(Some("t1_e"));
		tree.add(parse_fragment(MORE_REPLIES));

		let listing = tree.into_listing(9999);
		assert_eq!(ids(&listing), ["a"]);
		let a = &listing["data"]["children"][0]["data"];
		assert_eq!(ids(&a["replies"]), ["d"], "ancestors only keep the thread's branch");
		assert_eq!(ids(&a["replies"]["data"]["children"][0]["data"]["replies"]), ["e"]);
	}

	#[test]
	fn test_context_limits_ancestors() {
		let mut tree = tree(Some("t1_e"));
		tree.add(parse_fragment(MORE_REPLIES));
		assert_eq!(ids(&tree.into_listing(1)), ["d"]);
	}

	#[test]
	fn test_lineage_survives_parent_cycles() {
		let mut tree = CommentTree::new("t3_p".into(), Some("t1_z".into()));
		tree.add(parse_fragment(
			r#"<shreddit-comment thingId="t1_x" parentId="t1_y" depth="1"></shreddit-comment>
<shreddit-comment thingId="t1_y" parentId="t1_x" depth="1"></shreddit-comment>"#,
		));
		let x = "t1_x".to_string();
		assert_eq!(tree.lineage(&x).count(), 3);
	}

	#[test]
	fn test_more_stub_shape() {
		let listing = tree(None).into_listing(0);
		let stub = &listing["data"]["children"][2];
		assert_eq!(stub["kind"], "more");
		assert_eq!(stub["data"]["count"], 40);
		assert_eq!(stub["data"]["parent_id"], "t3_p");
		assert_eq!(stub["data"]["children"], json!([]));
	}

	#[tokio::test]
	async fn test_expand_adds_fragments_and_keeps_failures_as_stubs() {
		let calls = Arc::new(Mutex::new(Vec::new()));
		let mut tree = tree(None);
		expand(&mut tree, limits(100, 100, 10), fetcher(calls.clone())).await;

		assert_eq!(*calls.lock().unwrap(), ["c1", "c2", "c3"]);
		assert_eq!(tree.len(), 5);
		assert_eq!(tree.skipped.len(), 2, "failed expansions stay as stubs");

		let listing = tree.into_listing(0);
		assert_eq!(ids(&listing), ["a", "c", "_"]);
		assert_eq!(ids(&listing["data"]["children"][0]["data"]["replies"]), ["b", "d"]);
	}

	#[tokio::test]
	async fn test_expand_respects_expansion_budget() {
		let calls = Arc::new(Mutex::new(Vec::new()));
		let mut tree = tree(None);
		expand(&mut tree, limits(100, 2, 1), fetcher(calls.clone())).await;

		assert_eq!(*calls.lock().unwrap(), ["c1", "c2"]);
		assert_eq!(tree.pending.len(), 1, "the remaining placeholder is left unexpanded");
	}

	#[tokio::test]
	async fn test_expand_stops_at_comment_budget() {
		let calls = Arc::new(Mutex::new(Vec::new()));
		let mut tree = tree(None);
		expand(&mut tree, limits(3, 100, 10), fetcher(calls.clone())).await;

		assert!(calls.lock().unwrap().is_empty());
		assert_eq!(tree.pending.len(), 3);
	}

	#[tokio::test]
	async fn test_expand_in_focus_mode_skips_other_threads() {
		let calls = Arc::new(Mutex::new(Vec::new()));
		let mut tree = tree(Some("t1_b"));
		expand(&mut tree, limits(100, 100, 10), fetcher(calls.clone())).await;

		assert_eq!(*calls.lock().unwrap(), ["c1"]);
	}

	#[tokio::test]
	async fn test_expand_does_not_wait_for_slow_requests() {
		let (release_c1, c1_released) = tokio::sync::oneshot::channel::<()>();
		let release_c1 = Mutex::new(Some(release_c1));
		let c1_released = Mutex::new(Some(c1_released));
		let fetch = move |_path, cursor: String| -> std::pin::Pin<Box<dyn Future<Output = Result<String, String>> + Send>> {
			if cursor == "c1" {
				let released = c1_released.lock().unwrap().take().unwrap();
				return Box::pin(async move { released.await.map(|()| String::new()).map_err(|e| e.to_string()) });
			}
			if cursor == "c3" {
				release_c1.lock().unwrap().take().unwrap().send(()).unwrap();
			}
			Box::pin(std::future::ready(Ok(String::new())))
		};

		let mut tree = tree(None);
		let crawl = expand(&mut tree, limits(100, 100, 2), fetch);
		tokio::time::timeout(std::time::Duration::from_secs(5), crawl)
			.await
			.expect("c3 must start while c1 is still in flight");
	}

	#[test]
	fn test_parse_hover_card() {
		let html = r#"<faceplate-loader name="UserHoverCard_x"></faceplate-loader>
<div class="user-hover-card" data-testid="user-hover-card"><div><a href="/user/spez/"><span avatar><img src="https://styles.redditmedia.com/t5_3k30p/styles/profileIcon_a.png" alt="u/spez avatar"></span></a>
<a href="/user/spez/"><span dir="auto" data-testid="nameplate"> <span class="truncate">spez</span> </span></a>
<span dir="auto">u/spez</span><time datetime="2005-06-06T00:00:00.000Z" data-testid="cake-day"> Jun 6, 2005 </time></div>
<span>Reddit CEO</span>
<div><span data-testid="karma-number"> 184,587 </span><div> Post karma </div></div>
<div><span data-testid="karma-number"> 756,538 </span><div> Comment karma </div></div></div>"#;

		assert_eq!(
			parse_hover_card(html),
			Some(HoverCard {
				name: "spez".into(),
				avatar: "https://styles.redditmedia.com/t5_3k30p/styles/profileIcon_a.png".into(),
				created: 1_118_016_000.0,
				karma: 941_125,
			})
		);
	}

	#[test]
	fn test_parse_hover_card_missing_user() {
		let html = r#"<ac-call method="close" target="closest(faceplate-hovercard)" trigger="init"></ac-call><faceplate-alert level="error" message="We had a server error..." cause="query-bad-response"></faceplate-alert>"#;
		assert_eq!(parse_hover_card(html), None);
	}

	#[tokio::test]
	async fn test_user_hover_card_rejects_invalid_names() {
		for name in ["", "../comments", "a/b", "a?b"] {
			assert!(user_hover_card(name.to_string()).await.is_err(), "{name}");
		}
	}

	const SEARCH_FRAGMENT: &str = r#"
<search-telemetry-tracker data-faceplate-tracking-context="{&quot;post&quot;:{&quot;id&quot;:&quot;t3_abc&quot;,&quot;nsfw&quot;:false,&quot;spoiler&quot;:true,&quot;title&quot;:&quot;Fish &amp; chips &lt;3&quot;},&quot;profile&quot;:{&quot;id&quot;:&quot;t2_x&quot;,&quot;name&quot;:&quot;alice&quot;},&quot;subreddit&quot;:{&quot;id&quot;:&quot;t5_y&quot;,&quot;name&quot;:&quot;food&quot;,&quot;nsfw&quot;:false}}" view-events="search/view/post" data-testid="search-sdui-post" data-thingid="t3_abc"><div data-testid="search-post-unit">
  <search-telemetry-tracker data-faceplate-tracking-context="{}" click-events="search/click/post"><h2><a data-testid="post-title" href="/r/food/comments/abc/fish_chips/" aria-label="Fish &amp; chips &lt;3"></a></h2></search-telemetry-tracker>
  <div data-testid="sdui-post-unit">
    <div><span><faceplate-timeago format="narrow" ts="2024-05-02T13:00:55.859000+0000"></faceplate-timeago></span></div>
    <a data-testid="post-title-text" href="/r/food/comments/abc/fish_chips/">Fish &amp; chips</a>
    <div data-testid="search-counter-row"><span><faceplate-number number="1234" pretty></faceplate-number> votes</span><span>·</span><span><faceplate-number number="7" pretty></faceplate-number> comments</span></div>
  </div>
  <search-telemetry-tracker data-faceplate-tracking-context="{}" click-events="search/click/post"><a href="/r/food/comments/abc/fish_chips/"><faceplate-img data-testid="search_post_thumbnail" src="https://preview.redd.it/x.jpg?width=140&amp;height=78&amp;s=1"></faceplate-img></a></search-telemetry-tracker>
</div></search-telemetry-tracker>
<hr>
<search-telemetry-tracker data-faceplate-tracking-context="{&quot;post&quot;:{&quot;id&quot;:&quot;t3_def&quot;,&quot;nsfw&quot;:false,&quot;spoiler&quot;:false,&quot;title&quot;:&quot;Text post&quot;},&quot;subreddit&quot;:{&quot;name&quot;:&quot;food&quot;,&quot;nsfw&quot;:true}}" view-events="search/view/post"><div data-testid="search-post-with-content-preview">
  <h2><a data-testid="post-title" href="/r/food/comments/def/text_post/" aria-label="Text post"></a></h2>
  <div data-testid="sdui-post-unit">
    <a data-testid="post-title-text" href="/r/food/comments/def/text_post/">Text post</a>
    <search-telemetry-tracker data-faceplate-tracking-context="{}" click-events="search/click/post"><a href="/r/food/comments/def/text_post/">
      Body <b>preview</b>
    </a></search-telemetry-tracker>
    <div data-testid="search-counter-row"><span><faceplate-number number="2" pretty></faceplate-number> comments</span></div>
  </div>
</div></search-telemetry-tracker>
<search-telemetry-tracker data-faceplate-tracking-context="not json" view-events="search/view/post"></search-telemetry-tracker>
<faceplate-partial src="/svc/shreddit/community-hover-card/food" loading="programmatic"></faceplate-partial>
<faceplate-partial loading="lazy" src="/svc/shreddit/r/food/search/?q=fish&amp;cursor=eyJhIjoiYiJ9%3D%3D&amp;cId=1"></faceplate-partial>
"#;

	#[test]
	fn test_parse_search_fragment() {
		let page = parse_search_fragment(SEARCH_FRAGMENT);
		assert_eq!(page.cursor.as_deref(), Some("eyJhIjoiYiJ9=="));
		assert_eq!(page.posts.len(), 2);

		let link = &page.posts[0]["data"];
		assert_eq!(link["id"], "abc");
		assert_eq!(link["title"], "Fish & chips <3");
		assert_eq!(link["subreddit"], "food");
		assert_eq!(link["author"], "alice");
		assert_eq!(link["permalink"], "/r/food/comments/abc/fish_chips/");
		assert_eq!(link["score"], 1234);
		assert_eq!(link["hide_score"], false);
		assert_eq!(link["num_comments"], 7);
		assert_eq!(link["created_utc"], 1_714_654_855.859);
		assert_eq!(link["spoiler"], true);
		assert_eq!(link["over_18"], false);
		assert_eq!(link["is_self"], false);
		assert_eq!(link["thumbnail"], "https://preview.redd.it/x.jpg?width=140&height=78&s=1");
		assert_eq!((link["thumbnail_width"].as_i64(), link["thumbnail_height"].as_i64()), (Some(140), Some(78)));

		let text = &page.posts[1]["data"];
		assert_eq!(text["author"], "[deleted]");
		assert_eq!(text["over_18"], true);
		assert_eq!(text["is_self"], true);
		assert_eq!(text["hide_score"], true);
		assert_eq!(text["num_comments"], 2);
		// The site-wide snippet is often a matching comment, not the post's text.
		assert!(text["selftext_html"].is_null());
	}

	#[test]
	fn test_parse_search_fragment_without_results() {
		let page = parse_search_fragment(r#"<div data-testid="search-error-message">Hm...we couldn’t find any results</div>"#);
		assert!(page.posts.is_empty());
		assert_eq!(page.cursor, None);
	}

	#[test]
	fn test_search_path() {
		assert_eq!(search_path(None, "a b&c", "", "", None).unwrap(), "/svc/shreddit/search/?q=a+b%26c");
		assert_eq!(
			search_path(Some("rust"), "async", "top", "week", Some("eyJ+/==")).unwrap(),
			"/svc/shreddit/r/rust/search/?q=async&sort=top&t=week&cursor=eyJ%2B%2F%3D%3D"
		);
		assert_eq!(
			search_path(Some("rust+golang"), "async", "new", "", None).unwrap(),
			"/svc/shreddit/search/?q=async+%28subreddit%3Arust+OR+subreddit%3Agolang%29&sort=new"
		);
		for sub in ["../comments", "a/b", "a?b", "r%2F"] {
			assert!(search_path(Some(sub), "q", "", "", None).is_err(), "{sub}");
		}
	}
}
