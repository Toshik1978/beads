//! Property test: symmetric stateless echo suppression holds after every
//! sync, for any interleaving of local and remote comment additions.
//!
//! `plan_comment_sync` (`beads::remote::comments`) has no state to consult —
//! it decides push/pull purely from the comments in front of it, using the
//! `[br]` marker and the `youtrack` author as the only signals. The pinned
//! unit tests in `src/remote/comments.rs` check that three *specific*
//! consecutive syncs add nothing after the first; this test generates
//! arbitrary sequences of "add a local comment" / "add a remote comment" /
//! "run a sync" and checks the same invariant holds after every sync in
//! every generated sequence, not just the one hand-picked case.
//!
//! What this proves: the algorithm is idempotent and symmetric across
//! however local and remote additions are interleaved with sync points.
//! What it cannot prove: it says nothing about two *distinct* comments that
//! happen to carry identical text (content-based matching cannot tell those
//! apart, by design — see the module docs on `plan_comment_sync`), so this
//! generator gives every added comment unique, tagged text on purpose,
//! rather than exploring collisions that are a known, accepted limitation
//! rather than a bug this test is meant to catch.

use beads::remote::comments::{
    LocalComment, RemoteComment, YOUTRACK_AUTHOR, is_br_echo, is_youtrack_echo, plan_comment_sync,
};
use chrono::{DateTime, Utc};
use proptest::prelude::*;

#[derive(Debug, Clone, Copy)]
enum Op {
    AddLocal,
    AddRemote,
    Sync,
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![Just(Op::AddLocal), Just(Op::AddRemote), Just(Op::Sync)]
}

/// `RemoteComment::for_test` is `#[cfg(test)]`-only, so it is not linked into
/// this integration test binary. Every field is `pub`, so a struct literal
/// does the same job in three lines.
fn remote_comment(id: &str, text: &str, author_login: &str) -> RemoteComment {
    RemoteComment {
        id: id.to_string(),
        text: text.to_string(),
        author_login: author_login.to_string(),
        created: DateTime::<Utc>::UNIX_EPOCH,
    }
}

/// Non-`youtrack` bead comments must equal `[br]` YouTrack comments, and
/// `youtrack` bead comments must equal non-`[br]` YouTrack comments. See the
/// unit test of the same shape in `src/remote/comments.rs`.
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

proptest! {
    #[test]
    fn arbitrary_interleavings_preserve_the_echo_invariant(
        ops in proptest::collection::vec(op_strategy(), 0..40)
    ) {
        let mut local: Vec<LocalComment> = Vec::new();
        let mut remote: Vec<RemoteComment> = Vec::new();
        let mut local_seq = 0u32;
        let mut remote_seq = 0u32;

        for op in ops {
            match op {
                Op::AddLocal => {
                    local_seq += 1;
                    // Uniquely tagged so two distinct additions never
                    // collide on text — see the module doc's "what this
                    // cannot prove" note.
                    local.push(LocalComment {
                        author: "anton".to_string(),
                        text: format!("local-{local_seq}"),
                    });
                }
                Op::AddRemote => {
                    remote_seq += 1;
                    remote.push(remote_comment(
                        &format!("7-{remote_seq}"),
                        &format!("remote-{remote_seq}"),
                        "anton",
                    ));
                }
                Op::Sync => {
                    let plan = plan_comment_sync(&local, &remote);
                    for text in plan.to_push {
                        remote.push(remote_comment("7-x", &text, "anton"));
                    }
                    for comment in plan.to_pull {
                        local.push(LocalComment {
                            author: YOUTRACK_AUTHOR.to_string(),
                            text: comment.text.clone(),
                        });
                    }
                    assert_invariant(&local, &remote);
                }
            }
        }
    }
}
