//! "Move to…": the user types where a note (or one transcript segment) should go and the app
//! resolves it. Cheap first: [`resolve_local`] matches the text against the node tree with no
//! model call (an exact name, a full path, a unique prefix / substring). Only an ambiguous or
//! new target goes to a haiku request (`prompts/move.*.md`, [`move_request`]) whose answer is
//! either an existing node or ONE new project, executed by [`execute_move`].

use oats_core::store::name_key;
use oats_core::{Node, NodeKind, Origin, Store, StoreError};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::claude_cli::Request;
use crate::interpret::{render, render_tree, strip_delimiters};

pub const MOVE_SYSTEM: &str = include_str!("../prompts/move.system.md");
pub const MOVE_USER: &str = include_str!("../prompts/move.user.md");

/// Longest project name / description kept from the model (characters).
const MAX_NAME_CHARS: usize = 200;
const MAX_DESC_CHARS: usize = 500;

/// What is being moved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MoveTarget {
    /// A note (episode id).
    Episode(i64),
    /// One transcript segment.
    Segment(i64),
}

impl MoveTarget {
    /// The `kind` / `id` the brain reports in `Queued` / `Settled` for this target.
    pub fn kind_and_id(self) -> (&'static str, i64) {
        match self {
            MoveTarget::Episode(id) => ("move", id),
            MoveTarget::Segment(id) => ("move_segment", id),
        }
    }

    fn noun(self) -> &'static str {
        match self {
            MoveTarget::Episode(_) => "note",
            MoveTarget::Segment(_) => "segment",
        }
    }
}

/// "Work › Export rewrite › CSV" for a node.
pub fn node_path(nodes: &[Node], id: i64) -> String {
    let mut parts = Vec::new();
    let mut cur = nodes.iter().find(|n| n.id == id);
    while let Some(n) = cur {
        parts.push(n.name.clone());
        cur = n.parent_id.and_then(|p| nodes.iter().find(|x| x.id == p));
        if parts.len() > 8 {
            break;
        }
    }
    parts.reverse();
    parts.join(" › ")
}

/// The outcome of matching the typed text against the tree without a model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocalMove {
    /// Exactly one node: move there now.
    One(i64),
    /// Several nodes fit equally: the model decides.
    Many(Vec<i64>),
    /// Nothing matches: the model decides (probably a new project).
    None,
}

/// Strip a trailing kind word ("commerce project" → "commerce").
fn strip_kind_word(key: &str) -> &str {
    for w in [" project", " topic", " category"] {
        if let Some(s) = key.strip_suffix(w) {
            return s;
        }
    }
    key
}

fn outcome(mut ids: Vec<i64>) -> Option<LocalMove> {
    ids.sort_unstable();
    ids.dedup();
    match ids.len() {
        0 => None,
        1 => Some(LocalMove::One(ids[0])),
        _ => Some(LocalMove::Many(ids)),
    }
}

/// Match `text` against the (non-archived) tree: a full path ("Work › Commerce project",
/// "work/commerce"), else an exact name, else a unique prefix, else a unique substring among
/// projects and topics. Pure.
pub fn resolve_local(nodes: &[Node], text: &str) -> LocalMove {
    let live: Vec<&Node> = nodes.iter().filter(|n| !n.archived).collect();
    let key = name_key(text);
    if key.chars().count() < 2 {
        return LocalMove::None;
    }
    let bare = strip_kind_word(&key).to_owned();

    // a path: the last parts of the node's own path equal the typed parts
    let parts: Vec<String> = text
        .split(['›', '/', '>'])
        .map(name_key)
        .filter(|p| !p.is_empty())
        .collect();
    if parts.len() > 1 {
        let hits: Vec<i64> = live
            .iter()
            .filter(|n| {
                let mut cur = Some(**n);
                for (i, want) in parts.iter().rev().enumerate() {
                    let Some(c) = cur else { return false };
                    let have = name_key(&c.name);
                    let ok = have == *want || (i == 0 && have == strip_kind_word(want));
                    if !ok {
                        return false;
                    }
                    cur = c
                        .parent_id
                        .and_then(|p| live.iter().find(|x| x.id == p).copied());
                }
                true
            })
            .map(|n| n.id)
            .collect();
        if let Some(o) = outcome(hits) {
            return o;
        }
    }

    // an exact name (a category too: naming it is asking for it)
    let exact: Vec<i64> = live
        .iter()
        .filter(|n| {
            let k = name_key(&n.name);
            k == key || k == bare
        })
        .map(|n| n.id)
        .collect();
    if let Some(o) = outcome(exact) {
        return o;
    }

    // prefix, then substring, among projects and topics
    let inner: Vec<(&Node, String)> = live
        .iter()
        .filter(|n| n.kind != NodeKind::Category)
        .map(|n| (*n, name_key(&n.name)))
        .collect();
    for needle in [&bare, &key] {
        let prefix: Vec<i64> = inner
            .iter()
            .filter(|(_, k)| k.starts_with(needle.as_str()))
            .map(|(n, _)| n.id)
            .collect();
        if prefix.len() == 1 {
            return LocalMove::One(prefix[0]);
        }
        let any: Vec<i64> = inner
            .iter()
            .filter(|(_, k)| k.contains(needle.as_str()))
            .map(|(n, _)| n.id)
            .collect();
        if let Some(o) = outcome(any) {
            return o;
        }
    }
    LocalMove::None
}

pub fn move_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "action": { "type": "string", "enum": ["existing", "create"] },
            "node_id": { "type": ["integer", "null"] },
            "new_project": {
                "type": ["object", "null"],
                "properties": {
                    "name": { "type": "string", "maxLength": MAX_NAME_CHARS },
                    "parent_id": { "type": "integer" },
                    "description": { "type": "string", "maxLength": MAX_DESC_CHARS }
                },
                "required": ["name", "parent_id", "description"],
                "additionalProperties": false
            },
            "reason": { "type": "string", "maxLength": MAX_NAME_CHARS }
        },
        "required": ["action", "node_id", "new_project", "reason"],
        "additionalProperties": false
    })
}

/// The haiku request. `item` is the note's title and summary (or the segment's text),
/// `current` the node it is filed under now.
pub fn move_request(
    tree: &[Node],
    target: MoveTarget,
    item: &str,
    current: Option<i64>,
    text: &str,
    model: Option<String>,
) -> Request {
    let current = match current {
        Some(id) if tree.iter().any(|n| n.id == id) => {
            format!("{} [{id}]", node_path(tree, id))
        }
        _ => "nothing (unfiled)".to_owned(),
    };
    let item: String = strip_delimiters(item).chars().take(800).collect();
    Request {
        system_prompt: MOVE_SYSTEM.to_owned(),
        user_prompt: render(
            MOVE_USER,
            &[
                ("tree", &render_tree(tree, None)),
                ("item_kind", target.noun()),
                ("current", &current),
                ("item", &item),
                ("text", &strip_delimiters(text)),
            ],
        ),
        json_schema: Some(move_schema()),
        model,
        max_turns: None,
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct NewProject {
    pub name: String,
    pub parent_id: i64,
    #[serde(default)]
    pub description: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct MoveDecision {
    pub action: String,
    #[serde(default)]
    pub node_id: Option<i64>,
    #[serde(default)]
    pub new_project: Option<NewProject>,
    #[serde(default)]
    pub reason: String,
}

pub fn parse_move(v: &Value) -> Result<MoveDecision, serde_json::Error> {
    serde_json::from_value(v.clone())
}

/// What a move did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Moved {
    pub node_id: i64,
    pub created: bool,
    pub confirmation: String,
}

/// Apply the model's decision in one transaction. A node id that does not exist, a parent
/// that is missing, or an empty name is an `Err` (nothing written). A new
/// project is the user's own (`created_by = user`) and recorded as a `node_create` change.
pub fn execute_move(
    store: &Store,
    target: MoveTarget,
    decision: &MoveDecision,
    now: i64,
) -> Result<Moved, String> {
    let r = store.with_savepoint(|s| -> Result<Result<Moved, String>, StoreError> {
        // the target first: nothing is created for an item that is gone
        let present = match target {
            MoveTarget::Episode(id) => s.episode(id)?.is_some(),
            MoveTarget::Segment(id) => s.segment(id)?.is_some(),
        };
        if !present {
            return Ok(Err(format!("the {} no longer exists", target.noun())));
        }
        let (dest, created) = match decision.action.as_str() {
            "existing" => {
                let Some(id) = decision.node_id else {
                    return Ok(Err("the model named no node".to_owned()));
                };
                match s.node(id)? {
                    Some(n) if !n.archived => (n, false),
                    _ => return Ok(Err(format!("no such node {id}"))),
                }
            }
            "create" => {
                let Some(np) = &decision.new_project else {
                    return Ok(Err("the model proposed no project".to_owned()));
                };
                let name: String = np.name.trim().chars().take(MAX_NAME_CHARS).collect();
                if name.is_empty() {
                    return Ok(Err("the proposed project has no name".to_owned()));
                }
                let all = s.nodes()?;
                let Some(parent) = all.iter().find(|n| n.id == np.parent_id && !n.archived) else {
                    return Ok(Err(format!("no such parent {}", np.parent_id)));
                };
                let existed = s.find_node(Some(parent.id), &name)?.is_some();
                let desc: String = np.description.chars().take(MAX_DESC_CHARS).collect();
                let node = s.add_node(
                    Some(parent.id),
                    NodeKind::Project,
                    &name,
                    &desc,
                    Origin::User,
                )?;
                if !existed {
                    s.record_change(
                        None,
                        "node_create",
                        &format!("Created project '{}'", node.name),
                        &Value::Null,
                        &json!(node),
                        now,
                    )?;
                }
                (node, !existed)
            }
            other => return Ok(Err(format!("unknown action '{other}'"))),
        };
        let moved = match target {
            MoveTarget::Episode(id) => s.move_episode(id, dest.id, now)?,
            MoveTarget::Segment(id) => {
                s.set_segment_node(id, Some(dest.id))?;
                true
            }
        };
        let path = node_path(&s.nodes()?, dest.id);
        let confirmation = if created {
            format!("Created {path} and moved the {}", target.noun())
        } else if !moved {
            // the model picked the note's own home: nothing changed, say so
            format!("Already in {path}")
        } else {
            format!("Moved to {path}")
        };
        Ok(Ok(Moved {
            node_id: dest.id,
            created,
            confirmation,
        }))
    });
    match r {
        Ok(inner) => inner,
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(id: i64, parent: Option<i64>, kind: NodeKind, name: &str) -> Node {
        Node {
            id,
            parent_id: parent,
            kind,
            name: name.into(),
            description: String::new(),
            created_by: Origin::Auto,
            position: 0,
            archived: false,
        }
    }

    fn tree() -> Vec<Node> {
        vec![
            n(1, None, NodeKind::Category, "Work"),
            n(2, None, NodeKind::Category, "Admin"),
            n(3, Some(1), NodeKind::Project, "Commerce"),
            n(4, Some(1), NodeKind::Project, "Commerce platform migration"),
            n(5, Some(2), NodeKind::Project, "Apartment lease"),
            n(6, Some(3), NodeKind::Topic, "Pricing"),
            n(7, Some(4), NodeKind::Topic, "Pricing"),
            n(8, Some(1), NodeKind::Project, "Hiring"),
        ]
    }

    #[test]
    fn an_exact_name_wins_over_longer_names_containing_it() {
        assert_eq!(resolve_local(&tree(), "commerce"), LocalMove::One(3));
        assert_eq!(
            resolve_local(&tree(), "  Apartment   LEASE "),
            LocalMove::One(5)
        );
        assert_eq!(resolve_local(&tree(), "admin"), LocalMove::One(2));
    }

    #[test]
    fn a_unique_prefix_or_substring_matches() {
        assert_eq!(resolve_local(&tree(), "apart"), LocalMove::One(5));
        assert_eq!(resolve_local(&tree(), "lease"), LocalMove::One(5));
        assert_eq!(resolve_local(&tree(), "hir"), LocalMove::One(8));
    }

    #[test]
    fn ambiguous_or_unknown_text_needs_the_model() {
        // two "Pricing" topics
        assert_eq!(
            resolve_local(&tree(), "pricing"),
            LocalMove::Many(vec![6, 7])
        );
        // a prefix of two projects is not unique ("commerce p" prefixes only one: use "commerc")
        assert_eq!(resolve_local(&tree(), "commerce pl"), LocalMove::One(4));
        assert_eq!(
            resolve_local(&tree(), "new project for the lease renewal under admin"),
            LocalMove::None
        );
        assert_eq!(resolve_local(&tree(), "x"), LocalMove::None);
        let mut t = tree();
        t[4].archived = true;
        assert_eq!(resolve_local(&t, "apartment lease"), LocalMove::None);
    }

    #[test]
    fn a_full_path_resolves_through_the_parents() {
        assert_eq!(
            resolve_local(&tree(), "Work › Commerce project"),
            LocalMove::One(3)
        );
        assert_eq!(resolve_local(&tree(), "work/commerce"), LocalMove::One(3));
        assert_eq!(
            resolve_local(&tree(), "Work > Commerce > Pricing"),
            LocalMove::One(6)
        );
        assert_eq!(
            resolve_local(&tree(), "commerce platform migration/pricing"),
            LocalMove::One(7)
        );
    }

    #[test]
    fn the_request_carries_the_tree_the_item_and_the_text() {
        let req = move_request(
            &tree(),
            MoveTarget::Episode(9),
            "Lease call",
            Some(1),
            "new project for the lease",
            Some("haiku".into()),
        );
        assert!(req.user_prompt.contains("[5] Apartment lease"));
        assert!(
            req.user_prompt.contains("Work › Commerce [3]") || req.user_prompt.contains("Work [1]")
        );
        assert!(req.user_prompt.contains("new project for the lease"));
        assert!(req.user_prompt.contains("Lease call"));
        assert!(!req.user_prompt.contains("{{"));
        assert!(req.json_schema.is_some());
    }
}
