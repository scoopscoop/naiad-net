//! Query-time application of tag relations (ADR 0002): canonicalize raw tags
//! through siblings (aliases), then expand through parents (implications).
//!
//! Pure graph logic over tag ids — no DB, no I/O — so the closure is unit-tested
//! in isolation. The `db` layer loads the edges and resolves ids back to tags.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};

/// Sibling edges: `bad_tag_id -> ideal_tag_id`. At most one ideal per bad tag
/// (enforced by `UNIQUE(bad_tag_id, service_id)` in the schema).
pub type SiblingEdges = HashMap<i64, i64>;

/// Parent edges: `child_tag_id -> [parent_tag_id, ...]`.
///
/// Expansion looks these up by *canonical* tag id, so an implication attached to
/// a non-canonical (aliased) child does not fire once the child is canonicalized
/// — see [`effective_tags`]. A known limitation; in practice parents are attached
/// to ideal tags.
pub type ParentEdges = HashMap<i64, Vec<i64>>;

/// Follow the `bad -> ideal` sibling chain to its terminal ideal.
///
/// On a cycle, returns the lowest tag id in the cycle, so the canonical form is
/// stable regardless of which node the walk entered from.
#[must_use]
pub fn canonicalize(tag_id: i64, siblings: &SiblingEdges) -> i64 {
    let mut current = tag_id;
    let mut visited: HashSet<i64> = HashSet::new();
    while let Some(&next) = siblings.get(&current) {
        if !visited.insert(current) {
            // `current` was already seen, so it lies on a cycle. Walk the cycle
            // from here and return its lowest id (deterministic break).
            return cycle_min(current, siblings);
        }
        current = next;
    }
    current
}

/// Lowest id on the cycle that `start` lies on. Assumes `start` is on a cycle.
fn cycle_min(start: i64, siblings: &SiblingEdges) -> i64 {
    let mut min = start;
    let mut current = start;
    while let Some(&next) = siblings.get(&current) {
        if next == start {
            break;
        }
        min = min.min(next);
        current = next;
    }
    min
}

/// The effective tag set for a file: canonicalize each raw tag through siblings,
/// then expand parents transitively (canonicalizing each parent target), with a
/// visited set guaranteeing termination on parent cycles. Ordered and deduped.
#[must_use]
pub fn effective_tags(
    raw: &[i64],
    siblings: &SiblingEdges,
    parents: &ParentEdges,
) -> BTreeSet<i64> {
    let mut result = BTreeSet::new();
    let mut worklist: Vec<i64> = Vec::new();
    for &t in raw {
        let c = canonicalize(t, siblings);
        if result.insert(c) {
            worklist.push(c);
        }
    }
    while let Some(tag) = worklist.pop() {
        if let Some(ancestors) = parents.get(&tag) {
            for &p in ancestors {
                let pc = canonicalize(p, siblings);
                if result.insert(pc) {
                    worklist.push(pc);
                }
            }
        }
    }
    result
}

/// A capped list of related tag ids plus the true (uncapped) count. `total`
/// drives the popover's "… N more" row (`more = total - ids.len()`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RelationCapped {
    /// Related ids, at most `cap`, in ascending id order.
    pub ids: Vec<i64>,
    /// True count before the cap was applied.
    pub total: usize,
}

/// The three related-tag sections for one canonical tag: aliases (its sibling
/// preimage), parents (`Implies`), children (`Implied by`). Parent and child
/// ids are canonicalized; alias ids are the raw bad-tag preimage. The query
/// id itself is excluded from each list.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RelationSections {
    pub aliases: RelationCapped,
    pub parents: RelationCapped,
    pub children: RelationCapped,
}

/// Cap an id list to `cap` while retaining the true pre-cap count.
#[must_use]
fn cap_ids(mut ids: Vec<i64>, cap: usize) -> RelationCapped {
    let total = ids.len();
    ids.truncate(cap);
    RelationCapped { ids, total }
}

/// A merged relation graph with precomputed reverse indexes, built once and
/// shared across queries (see `Db::relation_graph`). `match_set` on this type
/// is neighborhood-scoped: the O(all-edges) reverse-parent and
/// sibling-preimage builds happen once here, not per predicate.
#[derive(Debug)]
pub struct RelationGraph {
    siblings: SiblingEdges,
    parents: ParentEdges,
    /// canonical parent -> canonical children that imply it.
    reverse_parents: HashMap<i64, Vec<i64>>,
    /// canonical terminal -> all bad tags that canonicalize to it.
    sibling_preimage: HashMap<i64, Vec<i64>>,
}

impl RelationGraph {
    /// Build the graph and its reverse indexes. Mirrors the edge-firing rules
    /// of [`effective_tags`]/[`match_set`] exactly: non-canonical child keys
    /// are skipped, ancestors are canonicalized.
    #[must_use]
    pub fn new(siblings: SiblingEdges, parents: ParentEdges) -> Self {
        let mut reverse_parents: HashMap<i64, Vec<i64>> = HashMap::new();
        for (child, ancestors) in &parents {
            let cc = canonicalize(*child, &siblings);
            if cc != *child {
                continue;
            }
            for ancestor in ancestors {
                let pc = canonicalize(*ancestor, &siblings);
                reverse_parents.entry(pc).or_default().push(cc);
            }
        }
        let mut sibling_preimage: HashMap<i64, Vec<i64>> = HashMap::new();
        for bad in siblings.keys() {
            let canon = canonicalize(*bad, &siblings);
            // Skip cycle members that canonicalize to themselves: a 2-node alias
            // cycle {A→B, B→A} resolves both to the cycle-min A, so without this
            // guard A would appear in its own preimage, causing double-counting
            // in `build_relation_completion` and self-alias in `relations_of`.
            if canon != *bad {
                sibling_preimage.entry(canon).or_default().push(*bad);
            }
        }
        Self {
            siblings,
            parents,
            reverse_parents,
            sibling_preimage,
        }
    }

    /// An empty graph: `match_set` degenerates to the singleton query id.
    #[must_use]
    pub fn empty() -> Self {
        Self::new(SiblingEdges::new(), ParentEdges::new())
    }

    #[must_use]
    pub fn siblings(&self) -> &SiblingEdges {
        &self.siblings
    }

    #[must_use]
    pub fn parents(&self) -> &ParentEdges {
        &self.parents
    }

    /// Canonical terminal id -> every bad (aliased) tag id that canonicalizes to
    /// it. Empty for tags with no aliases. Backs the completion overlay's
    /// count merge (a canonical row's merged count sums its preimage's raw
    /// counts) and the detail popover's "Aliases" section.
    #[must_use]
    pub fn sibling_preimage_map(&self) -> &HashMap<i64, Vec<i64>> {
        &self.sibling_preimage
    }

    /// Canonical parent id -> canonical children that imply it. Backs the
    /// popover's "Implied by" section.
    #[must_use]
    pub fn reverse_parents_map(&self) -> &HashMap<i64, Vec<i64>> {
        &self.reverse_parents
    }

    /// True iff `tag_id` (after canonicalization) has any alias, parent, or
    /// child. Drives the per-chip relations glyph with zero extra queries.
    #[must_use]
    pub fn has_relations(&self, tag_id: i64) -> bool {
        let c = canonicalize(tag_id, &self.siblings);
        let has_alias = self.sibling_preimage.get(&c).is_some_and(|v| !v.is_empty());
        let has_parent = self
            .parents
            .get(&c)
            .is_some_and(|ps| ps.iter().any(|p| canonicalize(*p, &self.siblings) != c));
        let has_child = self
            .reverse_parents
            .get(&c)
            .is_some_and(|v| v.iter().any(|ch| *ch != c));
        has_alias || has_parent || has_child
    }

    /// Capped alias/parent/child sections for `tag_id` (canonicalized). Parent
    /// targets are canonicalized and the query id itself is excluded, matching
    /// how `effective_tags` fires edges. `cap` truncates each `ids` list; each
    /// section's `total` is the true pre-cap count.
    #[must_use]
    pub fn relations_of(&self, tag_id: i64, cap: usize) -> RelationSections {
        let c = canonicalize(tag_id, &self.siblings);

        let mut aliases: Vec<i64> = self.sibling_preimage.get(&c).cloned().unwrap_or_default();
        aliases.sort_unstable();

        let parents: Vec<i64> = self
            .parents
            .get(&c)
            .map(|ps| {
                ps.iter()
                    .map(|p| canonicalize(*p, &self.siblings))
                    .filter(|p| *p != c)
                    .collect::<BTreeSet<i64>>()
                    .into_iter()
                    .collect()
            })
            .unwrap_or_default();

        let children: Vec<i64> = self
            .reverse_parents
            .get(&c)
            .map(|cs| {
                cs.iter()
                    .copied()
                    .filter(|ch| *ch != c)
                    .collect::<BTreeSet<i64>>()
                    .into_iter()
                    .collect()
            })
            .unwrap_or_default();

        RelationSections {
            aliases: cap_ids(aliases, cap),
            parents: cap_ids(parents, cap),
            children: cap_ids(children, cap),
        }
    }

    /// Neighborhood-scoped [`match_set`]: BFS over the precomputed reverse
    /// indexes only. Same result as the free function.
    #[must_use]
    pub fn match_set(&self, query: i64) -> BTreeSet<i64> {
        let qc = canonicalize(query, &self.siblings);
        let mut sources: HashSet<i64> = HashSet::new();
        let mut queue: VecDeque<i64> = VecDeque::new();
        sources.insert(qc);
        queue.push_back(qc);
        while let Some(tag) = queue.pop_front() {
            if let Some(children) = self.reverse_parents.get(&tag) {
                for &child in children {
                    if sources.insert(child) {
                        queue.push_back(child);
                    }
                }
            }
        }
        let mut result = BTreeSet::new();
        for source in sources {
            result.insert(source);
            if let Some(bads) = self.sibling_preimage.get(&source) {
                result.extend(bads.iter().copied());
            }
        }
        result
    }
}

/// All raw tag ids whose presence on a file makes `query` appear in that file's
/// effective tag set. This is the query-side inverse of [`effective_tags`]: the
/// query is canonicalized, then expanded backward through parents (every tag that
/// transitively implies it) and siblings (every alias that canonicalizes to one of
/// those tags). A file matches `query` iff its raw tag ids intersect this set.
/// Pure and cycle-safe.
#[must_use]
pub fn match_set(query: i64, siblings: &SiblingEdges, parents: &ParentEdges) -> BTreeSet<i64> {
    let qc = canonicalize(query, siblings);

    // Reverse of the parent graph, mirroring how `effective_tags` fires edges:
    // only keys that are already canonical are ever looked up in the forward
    // direction, so we skip non-canonical child entries to stay consistent.
    let mut reverse_parents: HashMap<i64, Vec<i64>> = HashMap::new();
    for (child, ancestors) in parents {
        let cc = canonicalize(*child, siblings);
        if cc != *child {
            // `effective_tags` canonicalizes before looking up parents, so this
            // edge fires (if at all) via the canonical key — don't add a
            // duplicate reverse entry here or it will fire for raw aliases.
            continue;
        }
        for ancestor in ancestors {
            let pc = canonicalize(*ancestor, siblings);
            reverse_parents.entry(pc).or_default().push(cc);
        }
    }

    // Canonical tags from which `qc` is reachable by parent edges (including `qc`).
    let mut sources: HashSet<i64> = HashSet::new();
    let mut queue: VecDeque<i64> = VecDeque::new();
    sources.insert(qc);
    queue.push_back(qc);
    while let Some(tag) = queue.pop_front() {
        if let Some(children) = reverse_parents.get(&tag) {
            for &child in children {
                if sources.insert(child) {
                    queue.push_back(child);
                }
            }
        }
    }

    // Reverse-sibling preimage: each bad tag grouped under its canonical terminal.
    // Guard: skip cycle members that canonicalize to themselves (same fix as in
    // `RelationGraph::new`) so the free-function path is consistent.
    let mut sibling_preimage: HashMap<i64, Vec<i64>> = HashMap::new();
    for bad in siblings.keys() {
        let canon = canonicalize(*bad, siblings);
        if canon != *bad {
            sibling_preimage.entry(canon).or_default().push(*bad);
        }
    }

    let mut result = BTreeSet::new();
    for source in sources {
        result.insert(source);
        if let Some(bads) = sibling_preimage.get(&source) {
            result.extend(bads.iter().copied());
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn siblings(pairs: &[(i64, i64)]) -> SiblingEdges {
        pairs.iter().copied().collect()
    }

    fn parents(pairs: &[(i64, i64)]) -> ParentEdges {
        let mut m = ParentEdges::new();
        for &(child, parent) in pairs {
            m.entry(child).or_default().push(parent);
        }
        m
    }

    #[test]
    fn canonicalize_no_edge_is_identity() {
        assert_eq!(canonicalize(7, &SiblingEdges::new()), 7);
    }

    #[test]
    fn canonicalize_follows_chain_to_terminal() {
        // 1 -> 2 -> 3 (terminal)
        let s = siblings(&[(1, 2), (2, 3)]);
        assert_eq!(canonicalize(1, &s), 3);
        assert_eq!(canonicalize(2, &s), 3);
        assert_eq!(canonicalize(3, &s), 3);
    }

    #[test]
    fn canonicalize_cycle_returns_min_from_any_entry() {
        // 3 -> 5 -> 3 cycle
        let s = siblings(&[(3, 5), (5, 3)]);
        assert_eq!(canonicalize(3, &s), 3);
        assert_eq!(canonicalize(5, &s), 3);
    }

    #[test]
    fn canonicalize_cycle_with_tail_returns_cycle_min_not_tail() {
        // tail 1 -> cycle 5 -> 6 -> 5; the smallest *cycle* id is 5, not the
        // smaller tail id 1.
        let s = siblings(&[(1, 5), (5, 6), (6, 5)]);
        assert_eq!(canonicalize(1, &s), 5);
    }

    #[test]
    fn canonicalize_self_loop_terminates() {
        // The db layer rejects bad == ideal, but the closure must not loop if a
        // self-edge ever reaches it.
        let s = siblings(&[(5, 5)]);
        assert_eq!(canonicalize(5, &s), 5);
    }

    #[test]
    fn effective_tags_expands_single_parent() {
        // raw {10}; 10 -> 20 (parent)
        let got = effective_tags(&[10], &SiblingEdges::new(), &parents(&[(10, 20)]));
        assert_eq!(got, BTreeSet::from([10, 20]));
    }

    #[test]
    fn effective_tags_expands_transitively() {
        // 10 -> 20 -> 30
        let p = parents(&[(10, 20), (20, 30)]);
        let got = effective_tags(&[10], &SiblingEdges::new(), &p);
        assert_eq!(got, BTreeSet::from([10, 20, 30]));
    }

    #[test]
    fn effective_tags_dedups_diamond() {
        // 10 -> 20, 10 -> 30, 20 -> 40, 30 -> 40
        let p = parents(&[(10, 20), (10, 30), (20, 40), (30, 40)]);
        let got = effective_tags(&[10], &SiblingEdges::new(), &p);
        assert_eq!(got, BTreeSet::from([10, 20, 30, 40]));
    }

    #[test]
    fn effective_tags_terminates_on_parent_cycle() {
        // 10 -> 20 -> 10 (parent loop)
        let p = parents(&[(10, 20), (20, 10)]);
        let got = effective_tags(&[10], &SiblingEdges::new(), &p);
        assert_eq!(got, BTreeSet::from([10, 20]));
    }

    #[test]
    fn effective_tags_canonicalizes_parent_targets() {
        // raw {10}; parent 10 -> 20; but 20 is a bad sibling of 99.
        let s = siblings(&[(20, 99)]);
        let p = parents(&[(10, 20)]);
        let got = effective_tags(&[10], &s, &p);
        // 20 is canonicalized to 99 before entering the set.
        assert_eq!(got, BTreeSet::from([10, 99]));
    }

    #[test]
    fn effective_tags_canonicalizes_raw_tags() {
        // raw {1}; 1 is a bad sibling of 2.
        let s = siblings(&[(1, 2)]);
        let got = effective_tags(&[1], &s, &ParentEdges::new());
        assert_eq!(got, BTreeSet::from([2]));
    }

    #[test]
    fn sibling_preimage_map_groups_aliases_under_canonical() {
        // 1 -> 3 and 2 -> 3 (two aliases of ideal 3); 4 is unrelated.
        let g = RelationGraph::new(siblings(&[(1, 3), (2, 3)]), ParentEdges::new());
        let pre = g.sibling_preimage_map();
        let mut bads = pre.get(&3).cloned().unwrap_or_default();
        bads.sort_unstable();
        assert_eq!(bads, vec![1, 2]);
        assert!(pre.get(&4).is_none(), "unrelated tag has no preimage entry");
    }

    #[test]
    fn relation_graph_match_set_agrees_with_free_function() {
        let s = siblings(&[(1, 2)]);
        let p = parents(&[(2, 3)]);
        let g = RelationGraph::new(s.clone(), p.clone());
        for q in 1..=3 {
            assert_eq!(g.match_set(q), match_set(q, &s, &p));
        }
    }

    #[test]
    fn relation_graph_empty_is_identity() {
        let g = RelationGraph::empty();
        assert_eq!(g.match_set(7), BTreeSet::from([7]));
    }

    #[test]
    fn match_set_no_relations_is_just_the_query() {
        let got = match_set(7, &SiblingEdges::new(), &ParentEdges::new());
        assert_eq!(got, BTreeSet::from([7]));
    }

    #[test]
    fn match_set_sibling_alias_and_canonical_agree() {
        // 1 (bad) -> 2 (ideal)
        let s = siblings(&[(1, 2)]);
        let p = ParentEdges::new();
        // Searching the canonical 2 matches files raw-tagged with 1 or 2.
        assert_eq!(match_set(2, &s, &p), BTreeSet::from([1, 2]));
        // Searching the alias 1 canonicalizes to 2 first, so same set.
        assert_eq!(match_set(1, &s, &p), BTreeSet::from([1, 2]));
    }

    #[test]
    fn match_set_parent_includes_children() {
        // child 10 -> parent 20
        let p = parents(&[(10, 20)]);
        // Searching the parent 20 must match files tagged with the child 10.
        assert_eq!(
            match_set(20, &SiblingEdges::new(), &p),
            BTreeSet::from([10, 20])
        );
        // Searching the child 10 matches only the child.
        assert_eq!(
            match_set(10, &SiblingEdges::new(), &p),
            BTreeSet::from([10])
        );
    }

    #[test]
    fn match_set_transitive_parents() {
        // 10 -> 20 -> 30
        let p = parents(&[(10, 20), (20, 30)]);
        // Searching 30 matches everything below it.
        assert_eq!(
            match_set(30, &SiblingEdges::new(), &p),
            BTreeSet::from([10, 20, 30])
        );
    }

    #[test]
    fn match_set_sibling_and_parent_interaction() {
        // sibling 1 -> 2 ; parent 2 -> 3
        let s = siblings(&[(1, 2)]);
        let p = parents(&[(2, 3)]);
        // Searching 3 matches files tagged with the alias 1, the canonical 2, or 3.
        assert_eq!(match_set(3, &s, &p), BTreeSet::from([1, 2, 3]));
    }

    #[test]
    fn match_set_terminates_on_cycles() {
        // sibling cycle 1<->2: each canonicalizes to the cycle min (1).
        let s = siblings(&[(1, 2), (2, 1)]);
        assert_eq!(
            match_set(1, &s, &ParentEdges::new()),
            BTreeSet::from([1, 2])
        );
        assert_eq!(
            match_set(2, &s, &ParentEdges::new()),
            BTreeSet::from([1, 2])
        );
        // parent cycle 3<->4: BFS terminates, both reachable from each other.
        let p = parents(&[(3, 4), (4, 3)]);
        assert_eq!(
            match_set(3, &SiblingEdges::new(), &p),
            BTreeSet::from([3, 4])
        );
        assert_eq!(
            match_set(4, &SiblingEdges::new(), &p),
            BTreeSet::from([3, 4])
        );
    }

    #[test]
    fn has_relations_true_for_alias_parent_child_only_false_otherwise() {
        // 1 alias-of 2; 2 parent 3 (so 3 has a child); 9 unrelated.
        let g = RelationGraph::new(siblings(&[(1, 2)]), parents(&[(2, 3)]));
        assert!(g.has_relations(1), "alias side");
        assert!(g.has_relations(2), "has alias (preimage) and a parent");
        assert!(g.has_relations(3), "has a child");
        assert!(!g.has_relations(9), "unrelated");
    }

    #[test]
    fn relations_of_sections_and_totals_respect_cap() {
        // ideal 10 with aliases 1,2,3; parents 20,21; child 30.
        let g = RelationGraph::new(
            siblings(&[(1, 10), (2, 10), (3, 10)]),
            parents(&[(10, 20), (10, 21), (30, 10)]),
        );
        let r = g.relations_of(10, 2);
        // aliases: 3 total, capped to 2.
        assert_eq!(r.aliases.total, 3);
        assert_eq!(r.aliases.ids.len(), 2);
        let mut all_aliases = g.relations_of(10, 10).aliases.ids;
        all_aliases.sort_unstable();
        assert_eq!(all_aliases, vec![1, 2, 3]);
        // parents: 20, 21.
        let mut parents_ids = r.parents.ids.clone();
        parents_ids.sort_unstable();
        assert_eq!(r.parents.total, 2);
        assert_eq!(parents_ids, vec![20, 21]);
        // children: 30.
        assert_eq!(r.children.total, 1);
        assert_eq!(r.children.ids, vec![30]);
    }

    #[test]
    fn relations_of_canonicalizes_query_through_alias() {
        // Querying the alias id 1 resolves the same sections as the canonical 10.
        let g = RelationGraph::new(siblings(&[(1, 10)]), parents(&[(10, 20)]));
        let via_alias = g.relations_of(1, 10);
        let via_canon = g.relations_of(10, 10);
        assert_eq!(via_alias, via_canon);
    }

    // ---- regression: 2-node alias cycle must not put canonical in its own preimage ----

    #[test]
    fn sibling_preimage_excludes_canonical_on_2node_cycle() {
        // Cross-service conflicting edges: 1→2, 2→1. cycle-min is 1.
        let s = siblings(&[(1, 2), (2, 1)]);
        let g = RelationGraph::new(s.clone(), ParentEdges::new());

        // canonicalize still resolves both members to cycle-min 1.
        assert_eq!(canonicalize(1, &s), 1, "cycle-min resolves to itself");
        assert_eq!(canonicalize(2, &s), 1, "non-min resolves to cycle-min");

        let pre = g.sibling_preimage_map();
        // The canonical (1) must NOT appear in its own preimage.
        let bads = pre.get(&1).cloned().unwrap_or_default();
        assert!(
            !bads.contains(&1),
            "canonical 1 must not be in its own sibling preimage; got {bads:?}"
        );
        // The non-min cycle member (2) must appear in the preimage.
        assert!(
            bads.contains(&2),
            "non-min cycle member 2 must be in preimage of 1; got {bads:?}"
        );

        // relations_of must exclude the canonical from the aliases section.
        let rel = g.relations_of(1, 10);
        assert!(
            !rel.aliases.ids.contains(&1),
            "aliases section must not contain the canonical itself; got {:?}",
            rel.aliases.ids
        );
    }

    #[test]
    fn sibling_preimage_excludes_canonical_on_2node_cycle_with_third_alias() {
        // 2-cycle {1→2, 2→1} plus a normal alias 3→1 (3 maps cleanly to cycle-min 1).
        let s = siblings(&[(1, 2), (2, 1), (3, 1)]);
        let g = RelationGraph::new(s, ParentEdges::new());

        let pre = g.sibling_preimage_map();
        let mut bads = pre.get(&1).cloned().unwrap_or_default();
        bads.sort_unstable();

        // 1 must not be in the preimage; 2 and 3 must be.
        assert!(
            !bads.contains(&1),
            "canonical 1 must not be in its own sibling preimage; got {bads:?}"
        );
        assert_eq!(
            bads,
            vec![2, 3],
            "preimage must contain the two non-self aliases"
        );

        let rel = g.relations_of(1, 10);
        let mut alias_ids = rel.aliases.ids.clone();
        alias_ids.sort_unstable();
        assert_eq!(alias_ids, vec![2, 3], "relations_of aliases must be [2, 3]");
        assert!(
            !alias_ids.contains(&1),
            "self must not be listed as own alias"
        );
    }

    #[test]
    fn match_set_agrees_with_effective_tags_oracle() {
        // Property test: for random small graphs and random file taggings, a file
        // matches query q via match_set (raw tags intersect match_set(q)) exactly
        // when canonicalize(q) is in the file's forward effective_tags closure.
        // Deterministic xorshift PRNG — no external dependency.
        let mut rng: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };

        for _ in 0..500 {
            let n = 1 + (next() % 8) as i64; // tag ids 1..=n
            let mut s = SiblingEdges::new();
            let mut p = ParentEdges::new();
            for bad in 1..=n {
                if next() % 3 == 0 {
                    let ideal = 1 + (next() % n as u64) as i64;
                    if ideal != bad {
                        s.insert(bad, ideal);
                    }
                }
            }
            for child in 1..=n {
                if next() % 2 == 0 {
                    let parent = 1 + (next() % n as u64) as i64;
                    if parent != child {
                        p.entry(child).or_default().push(parent);
                    }
                }
            }
            let files: Vec<Vec<i64>> = (0..5)
                .map(|_| (1..=n).filter(|_| next() % 2 == 0).collect())
                .collect();

            let g = RelationGraph::new(s.clone(), p.clone());
            for q in 1..=n {
                let ms = match_set(q, &s, &p);
                assert_eq!(
                    g.match_set(q),
                    ms,
                    "RelationGraph::match_set disagrees q={q} s={s:?} p={p:?}"
                );
                let qc = canonicalize(q, &s);
                for raw in &files {
                    let via_match = raw.iter().any(|r| ms.contains(r));
                    let via_oracle = effective_tags(raw, &s, &p).contains(&qc);
                    assert_eq!(
                        via_match, via_oracle,
                        "q={q} raw={raw:?} siblings={s:?} parents={p:?}"
                    );
                }
            }
        }
    }
}
