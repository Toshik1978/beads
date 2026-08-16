//! Comment fetch and push, gated on the count, and symmetric stateless echo
//! suppression.
//!
//! `RemoteIssue.comments_count` already arrives with the issue list (see
//! `crate::remote::youtrack::fetch::ISSUE_FIELDS`), so a pair whose counts
//! agree costs nothing further: [`comment_counts_agree`] is a plain integer
//! comparison, and callers are expected to skip [`fetch_comments`] entirely
//! when it returns `true`. Without that gate a workspace of 134 issues would
//! pay 134 comment requests per `status` run to learn nothing.
//!
//! ## Echo suppression
//!
//! Comments are one of only three fields where the remote is allowed to win
//! outright, which makes the direction of a sync ambiguous unless something
//! tells the two sides apart. Authorship cannot: a comment br pushes is
//! authored by the token owner, who is also the human using the web UI, so
//! `author` alone never distinguishes "br wrote this" from "a person wrote
//! this". There is also no sync log to consult — the suppression has to be
//! derivable from the comment itself, on both sides, with nothing stored.
//!
//! So each side marks its own writes and the other side reads the mark:
//!
//! - **Outbound.** Every comment br posts carries a leading [`BR_MARKER`]
//!   line. [`is_br_echo`] recognises it, and a fetch never turns a
//!   `[br]`-marked comment back into a bead comment. The marker must be the
//!   *first line*, not merely present, so a bead comment that quotes `[br]`
//!   mid-sentence is still ordinary content and still gets pushed.
//! - **Inbound.** Every comment br imports is stored locally authored by
//!   [`YOUTRACK_AUTHOR`], the integration user. [`is_youtrack_echo`]
//!   recognises it, and a push never posts a `youtrack`-authored comment
//!   back.
//!
//! [`plan_comment_sync`] applies both rules together, and both are matches
//! on *content*, not identity — there is nothing else to key on. That has a
//! real consequence for an edit on either side, in both directions:
//!
//! - A human editing a `[br]`-marked comment in the web UI does not change
//!   its first line, so [`is_br_echo`] still recognises it and a fetch still
//!   never turns it into a *new* bead comment. But the edited body no longer
//!   matches the local comment it came from, so the next push treats that
//!   local comment as unseen and posts it again — **appending a duplicate,
//!   not updating the one already there.**
//! - Symmetrically, editing a plain remote comment br has already pulled
//!   changes its text, so it no longer matches the `youtrack`-authored local
//!   copy, and the next pull **imports it again as a second local comment.**
//!
//! Neither case corrupts the count invariant's shape — a pushed comment
//! stays `[br]`-marked, a pulled one stays `youtrack`-authored — but both
//! produce a duplicate on the edited side, forever, on every sync after the
//! edit. This feature suppresses re-import/re-push of an *unedited* echo; it
//! does not reconcile edits to a comment that has already crossed, and it
//! has no way to tell "edited" from "brand new" without an identity to
//! compare. Whether a stable identity in the marker line could close this
//! without breaking statelessness is tracked separately as `bds-4r2.17`; the
//! marker format is not changed here.

use crate::remote::error::RemoteError;
use crate::remote::http::HttpClient;
use crate::remote::model::RemoteIssue;
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::HashSet;

/// `fields=` selector for the comment fetch — exactly what the echo rule
/// needs and nothing more: the id (to address a future removal), the text
/// (to test for the `[br]` marker), and the author's login (to test for the
/// integration user).
const COMMENT_FIELDS: &str = "id,text,author(login),created";

/// Page size for the comment fetch, and the threshold `fetch_comments` pages
/// past rather than trusting as complete. See `fetch_comments`.
const COMMENT_PAGE_SIZE: u32 = 500;

/// The leading line every comment br pushes carries, so a later fetch can
/// recognise its own echo. See the module docs.
pub const BR_MARKER: &str = "[br]";

/// The integration user's login. A local comment stored with this author was
/// imported from YouTrack, not typed by a human, and must never be pushed
/// back. See the module docs.
pub const YOUTRACK_AUTHOR: &str = "youtrack";

/// One comment as YouTrack reports it.
///
/// `created` is **epoch milliseconds**, like `RemoteIssue::updated`, and is
/// parsed into a `DateTime<Utc>` at the boundary for the same reason: nothing
/// downstream should have to remember the wire format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteComment {
    pub id: String,
    pub text: String,
    pub author_login: String,
    pub created: DateTime<Utc>,
}

impl RemoteComment {
    /// Build one directly, for tests that do not want to round-trip JSON.
    #[cfg(test)]
    #[must_use]
    pub fn for_test(id: &str, text: &str, author_login: &str) -> Self {
        Self {
            id: id.to_string(),
            text: text.to_string(),
            author_login: author_login.to_string(),
            created: DateTime::<Utc>::UNIX_EPOCH,
        }
    }
}

/// A bead comment, in exactly the shape [`plan_comment_sync`] needs to
/// decide push vs. skip — not the full storage `Comment`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalComment {
    pub author: String,
    pub text: String,
}

/// What a sync should do about one issue's comments: push these, pull those.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommentPlan {
    pub to_push: Vec<String>,
    pub to_pull: Vec<RemoteComment>,
}

/// Fetch every comment on `id_readable`, paged.
///
/// An issue with exactly `COMMENT_PAGE_SIZE` comments used to be refused
/// outright rather than trusted as complete or silently truncated — a
/// deliberate hard boundary, but one that made such an issue permanently
/// unsyncable: `comment_counts_agree` would never agree (the local count can
/// never equal a count `fetch_comments` refuses to produce), so every run hit
/// the same refusal forever. This loops on `$skip`/`$top` instead, the same
/// way `fetch::fetch_snapshot` and `tags::fetch_all_tags` do, stopping on the
/// first short page — so there is no boundary left to land on exactly.
///
/// # Errors
/// Returns whatever `http` returns on transport or HTTP failure, and
/// `RemoteError::Config` when a page's response body is not a JSON array or a
/// comment's `created` is not a representable instant. Both are genuinely
/// impossible responses, not a size the fetch merely declines to handle.
pub fn fetch_comments(
    http: &HttpClient,
    id_readable: &str,
) -> Result<Vec<RemoteComment>, RemoteError> {
    let mut comments = Vec::new();
    let mut skip = 0_u32;
    loop {
        let path = format!(
            "/api/issues/{id_readable}/comments?fields={COMMENT_FIELDS}&$skip={skip}&$top={COMMENT_PAGE_SIZE}"
        );
        let raw = http.get_json(&path, "comments")?;
        let items = raw.as_array().ok_or_else(|| {
            RemoteError::Config(format!(
                "the comment list for {id_readable} returned a JSON {}, not an array",
                json_kind(&raw)
            ))
        })?;
        let count = u32::try_from(items.len()).unwrap_or(u32::MAX);
        for item in items {
            comments.push(parse_comment(id_readable, item)?);
        }
        if count < COMMENT_PAGE_SIZE {
            return Ok(comments);
        }
        skip += COMMENT_PAGE_SIZE;
    }
}

/// Post `text` verbatim as a new comment on `id_readable`.
///
/// The text is written exactly as given — including the `[br]` marker line a
/// caller has already prefixed, if any. This function knows nothing about
/// echo suppression; see `is_br_echo`/`plan_comment_sync`.
///
/// # Errors
/// Returns whatever `http` returns on transport or HTTP failure.
pub fn push_comment(http: &HttpClient, id_readable: &str, text: &str) -> Result<(), RemoteError> {
    let path = format!("/api/issues/{id_readable}/comments?fields=id");
    http.post_json(&path, &serde_json::json!({ "text": text }), "comment")?;
    Ok(())
}

/// Whether `remote`'s comment count already agrees with the bead's.
///
/// This is the whole gate: callers skip `fetch_comments` for any pair where
/// this returns `true`.
#[must_use]
pub fn comment_counts_agree(remote: &RemoteIssue, local_count: u32) -> bool {
    remote.comments_count == local_count
}

/// True iff `text`'s **first line** is exactly the `[br]` marker.
///
/// A leading-line check, never a substring match: a bead comment that quotes
/// `[br]` mid-sentence is ordinary content, and treating it as an echo would
/// silently stop it from ever reaching the mirror. See the module docs.
#[must_use]
pub fn is_br_echo(text: &str) -> bool {
    text.lines().next() == Some(BR_MARKER)
}

/// True iff `author` is the integration user — this comment was imported by
/// br, not typed by a human, and must never be pushed back.
#[must_use]
pub fn is_youtrack_echo(author: &str) -> bool {
    author == YOUTRACK_AUTHOR
}

/// The body of a `[br]`-marked comment with the marker line removed, so it
/// can be compared against the local text it originated from.
///
/// `is_br_echo` recognises the marker line through `str::lines`, which
/// treats both `\n` and `\r\n` as a line ending; this has to strip the same
/// two forms; or a CRLF-normalising path (e.g. any comment that went through
/// a Windows client) would leave a `\r` on the front of the stripped body,
/// the body would never again match its local original, and the comment
/// would re-push on every subsequent sync — the same failure mode as an
/// edited echo, but reachable with no edit at all.
fn strip_marker(text: &str) -> &str {
    match text.strip_prefix(BR_MARKER) {
        Some(rest) => rest
            .strip_prefix("\r\n")
            .or_else(|| rest.strip_prefix('\n'))
            .unwrap_or(rest),
        None => text,
    }
}

/// Decide what to push and what to pull for one issue's comments.
///
/// Symmetric and stateless: neither side is told what happened on a previous
/// run, so each side works it out fresh from the comments in front of it.
///
/// - A local comment is pushed unless its author is [`YOUTRACK_AUTHOR`]
///   (it is br's own past pull) or `remote` already carries a `[br]`-marked
///   comment whose body, with the marker stripped, equals it (it is br's own
///   past push, echoed back).
/// - A remote comment is pulled unless it [`is_br_echo`] (br's own past
///   push) or `local` already carries a `youtrack`-authored comment with the
///   same text (it is br's own past pull, replayed).
///
/// **Matching is by exact text, not by identity — there is no id to key on
/// across the two sides.** Two distinct comments that happen to carry the
/// same text are indistinguishable from one being the other's echo. Push
/// "ship it", and a later, unrelated local comment that also reads "ship
/// it" matches the first one's already-pushed echo and is silently never
/// pushed. This needs no adversarial input, only a short, ordinary phrase
/// repeated by coincidence, and it is the worst outcome this function can
/// produce: a real comment simply disappears rather than duplicating. It is
/// a recorded, permanent limitation of content-based matching (see
/// `bds-4r2.17`), not a bug fixed here.
#[must_use]
pub fn plan_comment_sync(local: &[LocalComment], remote: &[RemoteComment]) -> CommentPlan {
    let pushed_bodies: HashSet<&str> = remote
        .iter()
        .filter(|comment| is_br_echo(&comment.text))
        .map(|comment| strip_marker(&comment.text))
        .collect();
    let pulled_bodies: HashSet<&str> = local
        .iter()
        .filter(|comment| is_youtrack_echo(&comment.author))
        .map(|comment| comment.text.as_str())
        .collect();

    let to_push = local
        .iter()
        .filter(|comment| !is_youtrack_echo(&comment.author))
        .filter(|comment| !pushed_bodies.contains(comment.text.as_str()))
        .map(|comment| format!("{BR_MARKER}\n{}", comment.text))
        .collect();

    let to_pull = remote
        .iter()
        .filter(|comment| !is_br_echo(&comment.text))
        .filter(|comment| !pulled_bodies.contains(comment.text.as_str()))
        .cloned()
        .collect();

    CommentPlan { to_push, to_pull }
}

fn parse_comment(id_readable: &str, raw: &Value) -> Result<RemoteComment, RemoteError> {
    let id = string_field(raw, "id");
    let millis = raw.get("created").and_then(Value::as_i64).unwrap_or(0);
    let created = DateTime::<Utc>::from_timestamp_millis(millis).ok_or_else(|| {
        RemoteError::Config(format!(
            "a comment on {id_readable} reports created={millis}, which is not a \
             representable instant"
        ))
    })?;
    let author_login = raw
        .get("author")
        .and_then(|author| author.get("login"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    Ok(RemoteComment {
        id,
        text: string_field(raw, "text"),
        author_login,
        created,
    })
}

fn string_field(raw: &Value, key: &str) -> String {
    raw.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Object(_) => "object",
        Value::Array(_) => "array",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::http::{HttpClient, RetryPolicy, Token};
    use test_support::mock_http::MockServer;

    const COMMENTS_PATH: &str =
        "/api/issues/EM-1/comments?fields=id,text,author(login),created&$skip=0&$top=500";

    fn comments_page_path(skip: u32) -> String {
        format!(
            "/api/issues/EM-1/comments?fields=id,text,author(login),created&$skip={skip}&$top=500"
        )
    }

    #[test]
    fn comment_counts_agree_is_an_equality_check() {
        // This pins the function itself: a plain `==`. It proves nothing
        // about requests — deleting the gate that skips `fetch_comments` on
        // `false` would not fail this test. That caller lands in `.8`'s
        // executor, and the "zero requests when counts agree" assertion
        // belongs to it, not here.
        let mut remote = RemoteIssue::for_test("EM-1");
        remote.comments_count = 3;
        assert!(comment_counts_agree(&remote, 3));
        assert!(!comment_counts_agree(&remote, 2));
    }

    #[test]
    fn a_fetch_requests_the_fields_the_echo_rule_needs() {
        let server = MockServer::start();
        server.on(
            "GET",
            COMMENTS_PATH,
            200,
            r#"[{"id":"7-1","text":"[br]\nhello","author":{"login":"anton"},"created":1786881856457}]"#,
        );
        let http = HttpClient::new(&server.base_url(), Token::new("t"), RetryPolicy::none());

        let comments = fetch_comments(&http, "EM-1").expect("fetch");

        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].text, "[br]\nhello");
        assert_eq!(comments[0].author_login, "anton");
        assert_eq!(comments[0].created.timestamp_millis(), 1_786_881_856_457);
    }

    #[test]
    fn a_push_posts_the_text_verbatim() {
        let server = MockServer::start();
        server.on(
            "POST",
            "/api/issues/EM-1/comments?fields=id",
            200,
            r#"{"id":"7-2"}"#,
        );
        let http = HttpClient::new(&server.base_url(), Token::new("t"), RetryPolicy::none());

        push_comment(&http, "EM-1", "[br]\nfrom beads").expect("push");

        let posts = server.write_requests();
        assert_eq!(posts.len(), 1);
        let body: serde_json::Value = serde_json::from_str(&posts[0].body).expect("json");
        assert_eq!(body["text"], "[br]\nfrom beads");
    }

    #[test]
    fn only_a_leading_marker_line_counts_as_an_echo() {
        assert!(is_br_echo("[br]\nhello from beads"));
        assert!(is_br_echo("[br]"));
        assert!(
            !is_br_echo("the marker is [br] and it goes first"),
            "a mid-text mention is an ordinary comment and must still be pushed"
        );
        assert!(!is_br_echo("hello\n[br]"), "the marker must lead");
    }

    #[test]
    fn each_side_ignores_its_own_echo() {
        let local = vec![
            LocalComment {
                author: "anton".into(),
                text: "local one".into(),
            },
            LocalComment {
                author: YOUTRACK_AUTHOR.into(),
                text: "pulled earlier".into(),
            },
        ];
        let remote = vec![
            RemoteComment::for_test("7-1", "[br]\nlocal one", "anton"),
            RemoteComment::for_test("7-2", "typed in the web UI", "anton"),
        ];

        let plan = plan_comment_sync(&local, &remote);

        assert!(
            plan.to_push.is_empty(),
            "nothing new locally: {:?}",
            plan.to_push
        );
        assert_eq!(plan.to_pull.len(), 1, "only the web-UI comment comes back");
        assert_eq!(plan.to_pull[0].id, "7-2");
    }

    #[test]
    fn a_new_local_comment_is_pushed_with_the_marker() {
        let local = vec![LocalComment {
            author: "anton".into(),
            text: "fresh".into(),
        }];
        let plan = plan_comment_sync(&local, &[]);
        assert_eq!(plan.to_push.len(), 1);
        assert!(
            plan.to_push[0].starts_with(BR_MARKER),
            "{}",
            plan.to_push[0]
        );
        assert!(plan.to_push[0].contains("fresh"));
    }

    #[test]
    fn a_pulled_comment_is_never_pushed_back() {
        let local = vec![LocalComment {
            author: YOUTRACK_AUTHOR.into(),
            text: "from the web".into(),
        }];
        let plan = plan_comment_sync(
            &local,
            &[RemoteComment::for_test("7-2", "from the web", "anton")],
        );
        assert!(
            plan.to_push.is_empty(),
            "an author=youtrack comment is our own echo"
        );
    }

    #[test]
    fn three_consecutive_syncs_add_nothing_after_the_first() {
        let mut local = vec![LocalComment {
            author: "anton".into(),
            text: "one".into(),
        }];
        let mut remote: Vec<RemoteComment> = vec![RemoteComment::for_test("7-9", "two", "anton")];

        for round in 0..3 {
            let plan = plan_comment_sync(&local, &remote);
            if round > 0 {
                assert!(
                    plan.to_push.is_empty(),
                    "round {round} pushed a duplicate: {:?}",
                    plan.to_push
                );
                assert!(
                    plan.to_pull.is_empty(),
                    "round {round} pulled a duplicate: {:?}",
                    plan.to_pull
                );
            }
            for text in plan.to_push {
                remote.push(RemoteComment::for_test("7-x", &text, "anton"));
            }
            for comment in plan.to_pull {
                local.push(LocalComment {
                    author: YOUTRACK_AUTHOR.into(),
                    text: comment.text.clone(),
                });
            }
            assert_invariant(&local, &remote);
        }
    }

    /// Non-`youtrack` bead comments == `[br]` YouTrack comments, and
    /// `youtrack` bead comments == non-`[br]` YouTrack comments.
    fn assert_invariant(local: &[LocalComment], remote: &[RemoteComment]) {
        let local_own = local
            .iter()
            .filter(|c| !is_youtrack_echo(&c.author))
            .count();
        let remote_ours = remote.iter().filter(|c| is_br_echo(&c.text)).count();
        assert_eq!(local_own, remote_ours, "local-origin counts disagree");

        let local_pulled = local.iter().filter(|c| is_youtrack_echo(&c.author)).count();
        let remote_theirs = remote.iter().filter(|c| !is_br_echo(&c.text)).count();
        assert_eq!(local_pulled, remote_theirs, "remote-origin counts disagree");
    }

    // --- Fix round 1 additions -------------------------------------------

    #[test]
    fn an_edit_to_an_already_pushed_comment_pushes_it_again() {
        // A human edits the body of a `[br]`-marked comment in the web UI.
        // It is still recognised as an echo (its first line is untouched),
        // but the edited body no longer matches the local comment it came
        // from, so the next sync treats that local comment as unseen and
        // pushes it again. See the module docs' "matches on content, not
        // identity" note; this is documented, not desired, behaviour.
        let local = vec![LocalComment {
            author: "anton".into(),
            text: "original text".into(),
        }];
        let remote = vec![RemoteComment::for_test(
            "7-1",
            "[br]\nedited in the web UI",
            "anton",
        )];

        let plan = plan_comment_sync(&local, &remote);

        assert_eq!(
            plan.to_push,
            vec!["[br]\noriginal text".to_string()],
            "the edited echo no longer matches, so the original is pushed again"
        );
    }

    #[test]
    fn an_edit_to_an_already_pulled_comment_pulls_it_again() {
        // Symmetric case: br has already pulled a plain remote comment into
        // a `youtrack`-authored local copy. A human then edits the remote
        // comment's text. It no longer matches the local copy, so the next
        // pull imports it again as a second local comment.
        let local = vec![LocalComment {
            author: YOUTRACK_AUTHOR.into(),
            text: "typed in the web UI".into(),
        }];
        let remote = vec![RemoteComment::for_test(
            "7-2",
            "typed in the web UI, then edited",
            "anton",
        )];

        let plan = plan_comment_sync(&local, &remote);

        assert_eq!(
            plan.to_pull.len(),
            1,
            "the edited remote text no longer matches the local copy, so it is pulled again: {:?}",
            plan.to_pull
        );
        assert_eq!(plan.to_pull[0].id, "7-2");
    }

    #[test]
    fn a_second_local_comment_with_identical_text_is_never_pushed() {
        // Documented limitation, not desired behaviour — see the doc on
        // `plan_comment_sync`. Matching is by exact text, so a second,
        // distinct comment that happens to repeat an already-pushed one's
        // wording is indistinguishable from that comment's own echo.
        let mut local = vec![LocalComment {
            author: "anton".into(),
            text: "ship it".into(),
        }];
        let mut remote: Vec<RemoteComment> = Vec::new();

        let first_sync = plan_comment_sync(&local, &remote);
        for text in first_sync.to_push {
            remote.push(RemoteComment::for_test("7-1", &text, "anton"));
        }

        // A human later writes a second, unrelated "ship it".
        local.push(LocalComment {
            author: "anton".into(),
            text: "ship it".into(),
        });

        let second_sync = plan_comment_sync(&local, &remote);
        assert!(
            second_sync.to_push.is_empty(),
            "known limitation: the second comment matches the first's echo and is dropped: {:?}",
            second_sync.to_push
        );
    }

    #[test]
    fn a_local_comment_that_literally_begins_with_the_marker_round_trips() {
        // A local comment whose own text happens to start with "[br]" gets a
        // second, outer marker line prefixed on push. The fetch-back is
        // still recognised as an echo, and `strip_marker` removes only that
        // outer marker line, recovering the original text — including its
        // own leading "[br]" — so it still matches and is not pushed twice.
        let local = vec![LocalComment {
            author: "anton".into(),
            text: "[br] not a marker, just my own text".into(),
        }];

        let plan = plan_comment_sync(&local, &[]);
        assert_eq!(plan.to_push.len(), 1);
        let pushed = plan.to_push[0].clone();
        assert_eq!(
            pushed, "[br]\n[br] not a marker, just my own text",
            "the local text's own leading marker must survive verbatim under the outer one"
        );

        let remote = vec![RemoteComment::for_test("7-1", &pushed, "anton")];
        let plan = plan_comment_sync(&local, &remote);
        assert!(
            plan.to_push.is_empty(),
            "the round trip must be recognised, not re-pushed: {:?}",
            plan.to_push
        );
    }

    #[test]
    fn a_crlf_marker_line_still_matches_its_local_original() {
        // `is_br_echo` recognises "[br]\r\n..." as an echo (`str::lines`
        // treats CRLF as one line ending); `strip_marker` must agree, or the
        // stripped body keeps a leading '\r', never matches the local
        // original again, and the comment re-pushes on every sync with no
        // edit involved.
        let local = vec![LocalComment {
            author: "anton".into(),
            text: "cross platform".into(),
        }];
        let remote = vec![RemoteComment::for_test(
            "7-1",
            "[br]\r\ncross platform",
            "anton",
        )];

        let plan = plan_comment_sync(&local, &remote);
        assert!(
            plan.to_push.is_empty(),
            "a CRLF-marked echo must still match its local original: {:?}",
            plan.to_push
        );
    }

    #[test]
    fn an_issue_with_five_hundred_and_one_comments_pages_past_the_boundary() {
        // The original bug: a page landing on exactly COMMENT_PAGE_SIZE was
        // refused outright, so an issue with precisely 500 comments never
        // synced at all — comment_counts_agree would never agree, and every
        // run hit the same refusal forever. This pins that the fetch now
        // pages instead, reaching a 501st comment on a second page.
        let server = MockServer::start();
        let first_page: Vec<serde_json::Value> = (0..500)
            .map(|n| {
                serde_json::json!({
                    "id": format!("7-{n}"),
                    "text": format!("comment {n}"),
                    "author": {"login": "anton"},
                    "created": 1_786_881_856_457_i64,
                })
            })
            .collect();
        server.on(
            "GET",
            &comments_page_path(0),
            200,
            &serde_json::Value::Array(first_page).to_string(),
        );
        server.on(
            "GET",
            &comments_page_path(500),
            200,
            r#"[{"id":"7-500","text":"the 501st comment","author":{"login":"anton"},"created":1786881856457}]"#,
        );
        let http = HttpClient::new(&server.base_url(), Token::new("t"), RetryPolicy::none());

        let comments = fetch_comments(&http, "EM-1").expect("fetch must page past the boundary");

        assert_eq!(comments.len(), 501, "both pages must be collected");
        assert_eq!(
            comments[500].text, "the 501st comment",
            "the comment beyond the old hard boundary must be visible"
        );
    }

    #[test]
    fn an_issue_with_exactly_five_hundred_comments_pages_to_an_empty_second_page() {
        // The headline case the previous test's name claimed but its fixture
        // (501 comments) did not actually exercise: a page landing on
        // *exactly* COMMENT_PAGE_SIZE, followed by a short (empty) second
        // page, rather than a second page carrying one more comment. Before
        // the fix this exact shape was the one that refused outright — a
        // full page could not be told apart from a truncated one — so it is
        // worth pinning on its own even though the 501-comment test already
        // proves the loop pages at all.
        let server = MockServer::start();
        let first_page: Vec<serde_json::Value> = (0..500)
            .map(|n| {
                serde_json::json!({
                    "id": format!("7-{n}"),
                    "text": format!("comment {n}"),
                    "author": {"login": "anton"},
                    "created": 1_786_881_856_457_i64,
                })
            })
            .collect();
        server.on(
            "GET",
            &comments_page_path(0),
            200,
            &serde_json::Value::Array(first_page).to_string(),
        );
        server.on("GET", &comments_page_path(500), 200, "[]");
        let http = HttpClient::new(&server.base_url(), Token::new("t"), RetryPolicy::none());

        let comments = fetch_comments(&http, "EM-1").expect("a full page must not be refused");

        assert_eq!(
            comments.len(),
            500,
            "a page landing exactly on the boundary must not be treated as suspicious"
        );
    }

    #[test]
    fn a_non_array_comment_body_is_still_an_error() {
        let server = MockServer::start();
        server.on("GET", COMMENTS_PATH, 200, r#"{"error":"Not Found"}"#);
        let http = HttpClient::new(&server.base_url(), Token::new("t"), RetryPolicy::none());

        let err = fetch_comments(&http, "EM-1").expect_err("must refuse a non-array body");
        let message = err.to_string();
        assert!(message.contains("EM-1"), "must name the issue: {message}");
    }
}
