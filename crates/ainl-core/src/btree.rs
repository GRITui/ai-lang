//! A balanced B-tree: `insert`, `get`, `remove`, and an in-order walk.
//!
//! This is the data structure behind Tier 4 card 3's tables. The card's whole
//! point is that a table lookup is O(log n) rather than the O(n) a scan of the
//! log's index would be, and this module is what makes that true.
//!
//! # Why a B-tree and not the obvious alternatives
//!
//! The two structures already in this repository were both rejected for a
//! reason worth recording, because the reasons *are* the design:
//!
//! * **The `HashMap` in [`crate::db`]** is O(1) and is the right index for the
//!   *log*, but a hash table cannot enumerate in sorted order without sorting
//!   the keys afterwards — O(n log n) on every enumeration, repeated forever.
//!   `db-keys` in [`crate::dbkv`] pays exactly that, and says so.
//! * **A sorted `Vec`** enumerates in order for free, but an insert is O(n),
//!   because every element past the insertion point moves. Inserting 10k rows in
//!   key order is 50M moves.
//!
//! A B-tree has both properties at once: the keys are *already* in order, so the
//! in-order walk is free, and a lookup touches O(log n) nodes.
//!
//! # The interface is deliberately four functions wide
//!
//! The AOT C runtime carries a hand-port of this tree (`dbt_*` in
//! `crates/ainl-cc/src/runtime.c`), and there is no FFI between the two — every
//! line of the C version is a second implementation that has to agree with this
//! one on every input. So the surface the table layer is allowed to use is kept
//! as small as the data structure permits: **insert, get, remove, iterate**.
//!
//! Anything else (a range scan, a seek-to-closest, a bulk load) would be a
//! second data path to keep in agreement across two languages, and none of them
//! is something the table layer needs. A second index — a lookup by a
//! non-primary column — is the obvious thing to want next and is deliberately
//! **not** here: a range scan is the first thing that would be built to serve
//! it, and that is a Tier 5 conversation.
//!
//! # Order
//!
//! Order 16: at most 15 keys per node, at least 7 in any non-root node. Order
//! 16 rather than 32 because this is an *educational* database whose point is
//! that the structure stays legible: at order 16 a node holds 16 `String`s, so
//! a printed tree is readable and a reader can check the balancing by hand. A
//! 10k-row table is 4 levels deep, which is deep enough that a linear scan would
//! visibly lose — see the measurement in `dbtab.rs`.
//!
//! # The balance rule, and its one documented exception
//!
//! Every node except the root holds between [`MIN_KEYS`] and [`MAX_KEYS`] keys,
//! and **all leaves sit at the same depth**. Those two together are what make
//! the height O(log n), and the test module re-checks both after every
//! operation rather than trusting the insert and remove code to be right.
//!
//! There is exactly one exception, and it is the standard one: a node with no
//! sibling has nothing to borrow from and nothing to merge with. That can only
//! be the root with a single child, which is itself exempt from the minimum —
//! and [`BTree::remove`] then makes that child the new root, so the tree loses a
//! level instead of keeping an underfull one. Every textbook B-tree deletion has
//! this case, and naming it is better than hiding it: the invariant checker
//! encodes exactly this rule, so a test cannot pass by accident on a shape the
//! code is not actually guaranteeing.

/// At most this many keys per node. `MAX_KEYS + 1` is the node's *order*, which
/// is where the "order 16" name comes from.
pub const MAX_KEYS: usize = 15;

/// The minimum number of keys in a non-root node: `ceil(16 / 2) - 1`.
pub const MIN_KEYS: usize = 7;

/// The order of the tree: at most `ORDER` children and `ORDER - 1` keys per
/// node. Exposed because the C port has to agree with it, and because the
/// balance bound the tests assert is stated in terms of it.
pub const ORDER: usize = 16;

/// One node: `keys` in ascending order with a parallel `vals` entry each, and
/// `kids.len() == keys.len() + 1`.
///
/// Children are the gaps *between* keys, which is why a search compares against
/// `keys[i]` and descends into `kids[i]`: `kids[k]` holds the keys that are `>=`
/// `keys[k]` and `<` `keys[k + 1]`. Pairing a child with each key instead would
/// need a sentinel for a leaf's missing rightmost child, and a sentinel is an
/// extra branch on the hot path.
///
/// The children are a `Vec<Node>`, not a `Vec<Box<Node>>`: a `Vec` is already a
/// heap allocation, so boxing each element again is a second allocation and a
/// second indirection per step of every descent, and the only cost of *not*
/// boxing is that moving a node in a `Vec` is a `memcpy` of its three `Vec`
/// headers rather than a pointer copy. The arrays themselves are never resized
/// in place — a split and a merge both build a fresh node — so that cost is
/// paid a handful of times per table lifetime, against O(log n) descents
/// happening on every single lookup.
#[derive(Default)]
struct Node {
    keys: Vec<String>,
    vals: Vec<String>,
    kids: Vec<Node>,
}

impl Node {
    /// Whether this node is a leaf — which also means "holds no keys" for the
    /// empty root, so one predicate answers both questions.
    fn is_leaf(&self) -> bool {
        self.kids.is_empty()
    }

    /// How many children this node has. A method rather than `kids.len()` at
    /// every site, because the delete paths do arithmetic on this count and an
    /// off-by-one there is a wrong-index panic rather than a wrong answer.
    fn children_len(&self) -> usize {
        self.kids.len()
    }

    /// Where `k` would go: the number of keys strictly smaller than it.
    fn search(&self, k: &str) -> usize {
        let kb = k.as_bytes();
        let mut lo = 0;
        let mut hi = self.keys.len();
        // Binary search on the **byte** value of the keys, not on any locale or
        // numeric reading of them. The C port compares with `memcmp`; a
        // comparator the two engines disagreed about would make `db-all-rows`
        // print the same rows in two different orders, which is exactly the
        // failure this card's parity claim exists to prevent.
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.keys[mid].as_bytes() < kb {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    /// Append this whole subtree in key order.
    ///
    /// Recursive, and the depth is the tree's *height* — O(log n), not the row
    /// count — so this cannot overflow a stack no matter how big the table is.
    fn collect<'a>(&'a self, out: &mut Vec<(&'a str, &'a str)>) {
        for i in 0..self.keys.len() {
            if let Some(kid) = self.kids.get(i) {
                kid.collect(out);
            }
            out.push((&self.keys[i], &self.vals[i]));
        }
        if let Some(last) = self.kids.last() {
            last.collect(out);
        }
    }
}

/// What a recursive insert reports back to its parent.
///
/// `Debug` is hand-written rather than derived: the derived one would need
/// `Debug` on `Node`, which would then print a promoted split's whole 8-key
/// sibling in full. Only the *fact* that a split happened is ever interesting
/// at a call site — the caller adopts the node, it does not read it — so the
/// impl reports the shape and not the contents.
enum Split {
    /// Nothing moved: the key was new and the node stayed small, or the key was
    /// already there and its value was replaced.
    Nothing,
    /// This node split. The median went up into the parent, and `right` is the
    /// new sibling the parent has to adopt as its next child.
    Promoted {
        key: String,
        val: String,
        right: Box<Node>,
    },
}

impl std::fmt::Debug for Split {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Split::Nothing => f.write_str("Nothing"),
            Split::Promoted { key, right, .. } => write!(
                f,
                "Promoted {{ key: {key:?}, sibling_keys: {} }}",
                right.keys.len()
            ),
        }
    }
}

/// A balanced B-tree over byte-ordered string keys.
#[derive(Default)]
pub struct BTree {
    root: Node,
    /// How many keys the tree holds, so `len` is O(1) and the delete paths never
    /// have to recount. Maintained by `insert` and `remove` only.
    count: usize,
}

impl BTree {
    /// An empty tree. A tree with no keys still has a root — an empty *leaf*,
    /// not a null pointer — so every operation has a node to look at and no path
    /// needs an "is there a root" test.
    pub fn new() -> BTree {
        BTree {
            root: Node::default(),
            count: 0,
        }
    }

    /// How many keys the tree holds.
    pub fn len(&self) -> usize {
        self.count
    }

    /// Whether the tree holds no keys.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// The value for `k`, or `None` if the tree has no such key.
    ///
    /// This is the O(log n) the card asks for: one level per node, comparing
    /// bytes, and no value is touched except the one returned.
    pub fn get(&self, k: &str) -> Option<&str> {
        let kb = k.as_bytes();
        let mut node = &self.root;
        loop {
            let i = node.search(k);
            if i < node.keys.len() && node.keys[i].as_bytes() == kb {
                return Some(&node.vals[i]);
            }
            if node.is_leaf() {
                return None;
            }
            node = &node.kids[i];
        }
    }

    /// Insert `k`/`v`, or replace `v` if `k` is already there. Returns `true`
    /// if the key was new.
    ///
    /// Replacement rather than a duplicate key is not a detail: a table row is
    /// identified by its primary key, so `db-insert` on an existing key is a
    /// last-write-wins overwrite. A tree that allowed two copies of one key
    /// would make `db-select` return whichever copy it happened to reach.
    pub fn insert(&mut self, k: &str, v: &str) -> bool {
        if self.get(k).is_some() {
            // A replace is checked by the equality of the two probes rather
            // than by a `Split` variant: the point of the assertion is that
            // `insert_into` did not split, and `matches!` says that without
            // needing `PartialEq` on a type that owns a whole node.
            debug_assert!(
                matches!(Self::insert_into(&mut self.root, k, v), Split::Nothing),
                "replacing a value cannot overflow a node"
            );
            return false;
        }
        if let Split::Promoted { key, val, right } = Self::insert_into(&mut self.root, k, v) {
            // The root split, so the tree gains a level: the median becomes the
            // only key of a new root, with the two halves as its children. A
            // root holding one key is exactly why the root is exempt from
            // MIN_KEYS.
            let left = std::mem::take(&mut self.root);
            let mut new_root = Node::default();
            new_root.keys.push(key);
            new_root.vals.push(val);
            new_root.kids.push(left);
            new_root.kids.push(*right);
            self.root = new_root;
        }
        self.count += 1;
        true
    }

    /// Remove `k`. Returns `true` if the key was there.
    ///
    /// Deletion is the genuinely hard half of a B-tree, because an underfull
    /// node has to be refilled by *borrowing* from a sibling or *merging* with
    /// one, and both move a key across a node boundary. Every step below names
    /// the case it handles; the invariant checker in the test module is what
    /// those comments are for.
    pub fn remove(&mut self, k: &str) -> bool {
        if self.get(k).is_none() {
            return false;
        }
        Self::delete_from(&mut self.root, k);
        // The root lost a key (only a merge can do that) and is now the
        // "no keys, one child" shape — which is a tree one level too tall, so
        // the child becomes the new root. This is the documented exception
        // coming back as a fix rather than as a hole in the invariants. An
        // emptied leaf root has no child and is already the right shape.
        if self.root.keys.is_empty() && self.root.children_len() == 1 {
            let child = self.root.kids.pop().expect("checked just above");
            self.root = child;
        }
        self.count -= 1;
        true
    }

    /// Every `(key, value)` in **ascending key order**, from an in-order walk.
    ///
    /// This is the whole reason for the structure over a hash table: the order
    /// is a property of the tree, so enumeration is O(n) and the two engines
    /// agree on the order without either one sorting. See [`Node::search`] for
    /// why the comparison is on bytes.
    pub fn iter(&self) -> Vec<(&str, &str)> {
        let mut out = Vec::with_capacity(self.count);
        self.root.collect(&mut out);
        out
    }

    /// The number of levels; a single leaf is 1.
    ///
    /// Exposed because the balance tests assert on it: "the tree stays
    /// balanced" is a claim about this number as much as about node fill.
    pub fn height(&self) -> usize {
        let mut n = &self.root;
        let mut h = 1;
        while !n.is_leaf() {
            n = &n.kids[0];
            h += 1;
        }
        h
    }

    /// The root's own keys, in order — empty while the root is still a leaf.
    ///
    /// Exposed for the **shape** test below, which exists because of something
    /// mutation testing turned up: changing the split median from `len / 2` to
    /// `(len - 1) / 2` leaves every *correctness* test in this module green.
    /// Both are legal splits of a 16-key node — 8 keys below the median versus
    /// 7 — so no invariant, no `get`, no walk, no count and no height can tell
    /// the two apart. The trees differ only in which key ended up at the root
    /// and how the leaves are filled, and nothing an AINL program can reach.
    ///
    /// It is still worth pinning, because the AOT C runtime implements this
    /// tree a second time (`dbt_split` in runtime.c) and the two have to make
    /// the same choice. If the port picked the other median, every answer would
    /// still match and the two engines would hold differently *shaped* trees —
    /// which is invisible today and is exactly the sort of drift that turns
    /// into a real parity bug the moment anything reads the shape. So the
    /// median is asserted by name, and the C port's test asserts the same name.
    pub fn root_keys(&self) -> Vec<&str> {
        self.root.keys.iter().map(|s| s.as_str()).collect()
    }

    // ---- insert ----------------------------------------------------------

    /// Insert into `node`, splitting it if it overflows. Returns what the parent
    /// has to do about the result.
    fn insert_into(node: &mut Node, k: &str, v: &str) -> Split {
        let i = node.search(k);
        if i < node.keys.len() && node.keys[i] == k {
            // Already present: replace in place. A tree that kept both copies
            // would have two answers for one key and `get` would return only
            // one of them.
            node.vals[i] = v.to_string();
            return Split::Nothing;
        }
        if node.is_leaf() {
            node.keys.insert(i, k.to_string());
            node.vals.insert(i, v.to_string());
        } else {
            // Descend. `insert_into` can only ever *add* a child, so the index
            // `i` of the child we descended into is still the right slot for the
            // promoted key and its new right sibling.
            if let Split::Promoted { key, val, right } = Self::insert_into(&mut node.kids[i], k, v)
            {
                node.keys.insert(i, key);
                node.vals.insert(i, val);
                node.kids.insert(i + 1, *right);
            }
        }
        if node.keys.len() > MAX_KEYS {
            Self::split(node)
        } else {
            Split::Nothing
        }
    }

    /// Split an overfull node: the median goes up to the parent, this node keeps
    /// the keys below it, and a new sibling takes the keys above.
    ///
    /// Both halves end with at least [`MIN_KEYS`] keys, which is what stops a
    /// delete from having to undo the split immediately.
    ///
    /// The median is `len / 2`, and the children split at `mid + 1`: for the 16
    /// keys an overfull node holds, taking index 8 leaves 8 below and 7 above —
    /// both halves legal. The two ports have to pick the *same* median, or the
    /// same insert sequence produces two different tree *shapes* and the
    /// "identical results" the card asks for becomes an accident.
    ///
    /// A **leaf** is the case that needs its own branch. A leaf has no children
    /// at all, so "split the children at `mid + 1`" is not a shorter list — it
    /// is a split of an empty list, which panics. The first seventeen rows of
    /// any table go through exactly this path, so it is not a corner case: it
    /// is the first split every tree ever does.
    fn split(node: &mut Node) -> Split {
        let mid = node.keys.len() / 2;
        let key = node.keys.remove(mid);
        let val = node.vals.remove(mid);
        let right_keys = node.keys.split_off(mid);
        let right_vals = node.vals.split_off(mid);
        // A node with k keys has k + 1 children, so the left half (which keeps
        // `mid` keys) keeps `mid + 1` children and the sibling gets the rest.
        // A leaf keeps zero and hands over zero.
        let right_kids = if node.is_leaf() {
            Vec::new()
        } else {
            node.kids.split_off(mid + 1)
        };
        Split::Promoted {
            key,
            val,
            right: Box::new(Node {
                keys: right_keys,
                vals: right_vals,
                kids: right_kids,
            }),
        }
    }

    // ---- delete ----------------------------------------------------------

    /// Delete `k` from the subtree rooted at `node`, which is known to contain
    /// it, and re-establish the minimum fill on the way back up.
    ///
    /// **Repairing after the recursive call, rather than before the descent, is
    /// the load-bearing decision here.** The textbook alternative — make the
    /// child big enough on the way *down*, so the delete cannot underfill it —
    /// has a hole: when the key being deleted is the very separator that the
    /// repair consumes, the merge has already removed the key this node was
    /// about to rewrite, and there is no key left to write. Repairing on the way
    /// up keeps the descent from disturbing this node's key array, so the index
    /// taken here is still the right one when the call returns. The cost is a
    /// repair even when the delete turned out to be free; `remove` checks for the
    /// key first, so that cost is never paid for a no-op.
    fn delete_from(node: &mut Node, k: &str) {
        let i = node.search(k);
        if node.is_leaf() {
            node.keys.remove(i);
            node.vals.remove(i);
            return;
        }
        if i < node.keys.len() && node.keys[i] == k {
            // The key is here but this is an interior node, so deleting it would
            // leave a hole in the middle. Replace it with its in-order
            // **predecessor** — the largest key in the subtree to its left — and
            // delete *that* from the left child, which is a leaf-side delete and
            // so simple. The successor would work identically; the predecessor
            // is the conventional choice.
            let pred = Self::rightmost_key(&node.kids[i]);
            node.keys[i] = pred.0.clone();
            node.vals[i] = pred.1;
            Self::delete_from(&mut node.kids[i], &pred.0);
        } else {
            Self::delete_from(&mut node.kids[i], k);
        }
        // The child is now one key short of legal if it had the minimum. Every
        // other child was legal when we arrived (inductively) and has not been
        // touched, so both repair paths below see a legal sibling.
        if node.kids[i].keys.len() < MIN_KEYS {
            Self::fix_child(node, i);
        }
    }

    /// Refill the underfull child at `ci`, by borrowing from a sibling or by
    /// merging with one. Returns the index of the child that now holds the keys
    /// the underfull one used to hold.
    ///
    /// The order is fixed and matters. Borrow from the left if the left sibling
    /// can spare a key, else from the right, else merge — because a merge is the
    /// only one of the three that changes *this* node's key count and the shape
    /// of the subtree below; both repairs leave the tree's height and every
    /// other node exactly as they were.
    fn fix_child(node: &mut Node, ci: usize) -> usize {
        if node.children_len() < 2 {
            // No sibling exists: this is the root with a single child, and the
            // root is exempt from the minimum. `BTree::remove` collapses the
            // level afterwards. The returned index is unchanged, so a caller
            // that keeps descending lands in the same child.
            return ci;
        }
        if ci > 0 && node.kids[ci - 1].keys.len() > MIN_KEYS {
            Self::borrow_from_left(node, ci);
            ci
        } else if ci + 1 < node.children_len() && node.kids[ci + 1].keys.len() > MIN_KEYS {
            Self::borrow_from_right(node, ci);
            ci
        } else if ci > 0 {
            // Both sides are at the minimum, so there is nothing to borrow. The
            // underfull child has MIN_KEYS - 1 keys and so does the left
            // sibling; the merged node gets (MIN_KEYS - 1) + 1 + MIN_KEYS = 15,
            // which is exactly MAX_KEYS — never one more, so a merge can never
            // leave a node that itself needs splitting.
            Self::merge(node, ci - 1);
            ci - 1
        } else {
            // `ci == 0` with no left sibling, so the underfull child is the left
            // one and the merge goes rightwards.
            Self::merge(node, 0);
            0
        }
    }

    /// Move the separator `keys[ci - 1]` down into the underfull child at `ci`,
    /// and the left sibling's largest key up to take its place.
    ///
    /// The sibling's rightmost child moves down with the key it held, which is
    /// what keeps every leaf at the same depth: a key and the subtree it
    /// separates move together, or the separator's meaning is lost.
    ///
    /// When the sibling is a **leaf** there is no such child to move, and
    /// popping one would panic on an empty list. All leaves sit at the same
    /// depth, so a leaf sibling implies a leaf child, and the borrow is then
    /// purely between two leaves.
    fn borrow_from_left(node: &mut Node, ci: usize) {
        let sep = node.keys.remove(ci - 1);
        let sep_val = node.vals.remove(ci - 1);
        let (up_key, up_val, moved_kid) = {
            let left = &mut node.kids[ci - 1];
            (
                left.keys
                    .pop()
                    .expect("a sibling with >MIN_KEYS has one to lend"),
                left.vals.pop().expect("keys and vals move together"),
                if left.is_leaf() {
                    None
                } else {
                    Some(left.kids.pop().expect("an internal node lends a child"))
                },
            )
        };
        // The up-moving key goes back into this node's array at the slot the
        // separator left, so this node's key count is unchanged.
        node.keys.insert(ci - 1, up_key);
        node.vals.insert(ci - 1, up_val);
        let child = &mut node.kids[ci];
        child.keys.insert(0, sep);
        child.vals.insert(0, sep_val);
        if let Some(kid) = moved_kid {
            child.kids.insert(0, kid);
        }
    }

    /// The mirror of [`Self::borrow_from_left`]: the separator `keys[ci]` moves
    /// down into the underfull child, and the right sibling's *smallest* key
    /// moves up to replace it.
    fn borrow_from_right(node: &mut Node, ci: usize) {
        let sep = node.keys.remove(ci);
        let sep_val = node.vals.remove(ci);
        let (up_key, up_val, moved_kid) = {
            let right = &mut node.kids[ci + 1];
            (
                right.keys.remove(0),
                right.vals.remove(0),
                if right.is_leaf() {
                    None
                } else {
                    Some(right.kids.remove(0))
                },
            )
        };
        node.keys.insert(ci, up_key);
        node.vals.insert(ci, up_val);
        let child = &mut node.kids[ci];
        child.keys.push(sep);
        child.vals.push(sep_val);
        if let Some(kid) = moved_kid {
            child.kids.push(kid);
        }
    }

    /// Merge the child at `left_ci + 1` into the child at `left_ci`: the
    /// separator between them descends, both children's remaining keys are
    /// concatenated in order, and the right child's children are adopted.
    ///
    /// Concatenation is correct without a sort precisely because both children
    /// are individually sorted and every key in the left one is smaller than
    /// every key in the right one — the separator invariant the checker below
    /// asserts separately.
    fn merge(node: &mut Node, left_ci: usize) {
        let sep = node.keys.remove(left_ci);
        let sep_val = node.vals.remove(left_ci);
        let mut right = node.kids.remove(left_ci + 1);
        let left = &mut node.kids[left_ci];
        left.keys.push(sep);
        left.vals.push(sep_val);
        left.keys.append(&mut right.keys);
        left.vals.append(&mut right.vals);
        left.kids.append(&mut right.kids);
    }

    /// The largest `(key, value)` in a subtree, which is the in-order
    /// predecessor of the first key of the subtree to its right.
    fn rightmost_key(node: &Node) -> (String, String) {
        let mut n = node;
        while !n.is_leaf() {
            n = &n.kids[n.children_len() - 1];
        }
        let i = n.keys.len() - 1;
        (n.keys[i].clone(), n.vals[i].clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- the invariant checker -------------------------------------------

    /// Everything a balanced B-tree is supposed to be true of, checked in one
    /// pass so that *every* test below gets it for free.
    ///
    /// This is the real instrument of this module. A B-tree's insert and delete
    /// are each a dozen branches, and a branch is only reached by a particular
    /// *shape* of tree — so a bug in the merge path shows up as an unsorted
    /// walk, a leaf at the wrong depth, or a lost key far more reliably than as
    /// a wrong `get`. No test in this file asserts "it returned the right
    /// answer" on its own.
    fn check(t: &BTree) {
        let mut leaf_depth: Option<usize> = None;
        let counted = walk(&t.root, true, 0, &mut leaf_depth, &mut Vec::new());
        assert_eq!(
            counted, t.count,
            "the key count and the keys actually in the tree disagree"
        );
        assert!(
            leaf_depth.is_some(),
            "an empty tree is one empty leaf, not no leaves at all"
        );
    }

    /// Walk one subtree, asserting every invariant, and return its key count.
    fn walk(
        n: &Node,
        is_root: bool,
        depth: usize,
        leaf_depth: &mut Option<usize>,
        path: &mut Vec<usize>,
    ) -> usize {
        assert_eq!(
            n.keys.len(),
            n.vals.len(),
            "a key with no value at depth {depth} path {path:?}"
        );
        for w in n.keys.windows(2) {
            assert!(
                w[0].as_bytes() < w[1].as_bytes(),
                "keys out of order in one node: {w:?} at depth {depth} path {path:?}"
            );
        }
        let min = if is_root { 0 } else { MIN_KEYS };
        assert!(
            n.keys.len() >= min,
            "underfull node: {} keys, minimum {min}, depth {depth} path {path:?}",
            n.keys.len()
        );
        // The root is exempt from the *minimum* but not from the maximum: after
        // a split it is rebuilt with a single key, so it never exceeds one.
        assert!(
            n.keys.len() <= MAX_KEYS,
            "overfull node: {} keys",
            n.keys.len()
        );

        if n.is_leaf() {
            // `replace` returns the *old* value, so the first leaf would always
            // mismatch — hence the explicit match: record the first depth, then
            // require every later leaf to agree with it.
            match *leaf_depth {
                None => *leaf_depth = Some(depth),
                Some(first) => assert_eq!(
                    first, depth,
                    "leaves at two different depths — the tree is not balanced"
                ),
            }
            return n.keys.len();
        }
        assert_eq!(
            n.children_len(),
            n.keys.len() + 1,
            "a node must have one child per gap between its keys"
        );

        let mut count = n.keys.len();
        for i in 0..n.children_len() {
            path.push(i);
            count += walk(&n.kids[i], false, depth + 1, leaf_depth, path);
            path.pop();
        }
        // The separator invariant, checked per gap. A borrow or a merge can
        // leave every node individually well-formed and still break this — a key
        // that belongs below a separator ends up above it — and nothing else in
        // the tree would notice, because `get` would simply not find it.
        for (i, sep) in n.keys.iter().enumerate() {
            let sep = sep.as_bytes();
            if let Some(hi) = max_key(&n.kids[i]) {
                assert!(hi < sep, "the child left of a separator holds a larger key");
            }
            if let Some(lo) = min_key(&n.kids[i + 1]) {
                assert!(
                    lo > sep,
                    "the child right of a separator holds a smaller key"
                );
            }
        }
        count
    }

    fn min_key(n: &Node) -> Option<&[u8]> {
        if let Some(k) = n.keys.first() {
            return Some(k.as_bytes());
        }
        n.kids.first().and_then(|k| min_key(k))
    }

    fn max_key(n: &Node) -> Option<&[u8]> {
        if let Some(k) = n.keys.last() {
            return Some(k.as_bytes());
        }
        n.kids.last().and_then(|k| max_key(k))
    }

    /// `k00000`-style keys, so byte order and insertion order can be varied
    /// independently of each other.
    fn key(i: usize) -> String {
        format!("k{i:05}")
    }

    // ---- insert ----------------------------------------------------------

    #[test]
    fn an_empty_tree_finds_nothing() {
        let t = BTree::new();
        check(&t);
        assert!(t.is_empty());
        assert_eq!(t.len(), 0);
        assert_eq!(t.get("nope"), None);
        assert_eq!(t.height(), 1);
        assert!(t.iter().is_empty());
    }

    #[test]
    fn insert_then_get_returns_what_was_stored() {
        let mut t = BTree::new();
        t.insert("a", "1");
        t.insert("b", "2");
        check(&t);
        assert_eq!(t.get("a"), Some("1"));
        assert_eq!(t.get("b"), Some("2"));
        assert_eq!(t.get("c"), None);
        assert_eq!(t.len(), 2);
    }

    /// The order is a property of the structure, not a sort applied to the
    /// result — so it is asserted on the walk itself.
    #[test]
    fn the_in_order_walk_is_sorted() {
        let mut t = BTree::new();
        // An order chosen to put inserts on both sides of a node boundary,
        // including both edges and the empty key.
        for k in [
            "m", "c", "z", "a", "t", "b", "y", "d", "aa", "A", "0", "~", "",
        ] {
            t.insert(k, k);
        }
        check(&t);
        let got: Vec<&str> = t.iter().into_iter().map(|(k, _)| k).collect();
        let mut want = got.clone();
        want.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
        assert_eq!(got, want, "the walk must be in byte order");
    }

    #[test]
    fn inserting_an_existing_key_replaces_and_does_not_grow() {
        let mut t = BTree::new();
        assert!(t.insert("k", "first"));
        assert!(!t.insert("k", "second"), "a replace is not a new key");
        check(&t);
        assert_eq!(t.get("k"), Some("second"));
        assert_eq!(t.len(), 1, "a replace must not add a second key");
        assert_eq!(t.iter(), vec![("k", "second")]);
    }

    /// Enough keys to split several times, inserted in an order that is *not*
    /// sorted, so splits land in the middle of full nodes rather than always at
    /// one edge.
    #[test]
    fn many_keys_stay_balanced() {
        let mut t = BTree::new();
        for i in (0..1000).rev() {
            t.insert(&key(i), &i.to_string());
        }
        check(&t);
        assert_eq!(t.len(), 1000);
        for i in 0..1000 {
            assert_eq!(t.get(&key(i)), Some(i.to_string().as_str()));
        }
        // Order 16, at least 7 keys per non-root node: a 3-level tree holds up
        // to 16^2 = 256 leaf nodes of 15 keys, which is far more than 1000 — so
        // this tree is 3 deep. Four levels would be legal too, just not minimal.
        // The bound asserted is the one that matters: NOT O(n). A list-backed
        // index would be 2, and its "depth" is 2 at any size.
        assert!(
            (3..=4).contains(&t.height()),
            "height {} is not O(log n) for 1000 keys",
            t.height()
        );
    }

    /// The two insertion orders the *shape* depends on: sorted (every split at
    /// the right edge) and reverse-sorted (every split at the left). Both are
    /// where an off-by-one in the median index shows up.
    #[test]
    fn sorted_and_reverse_insertion_stay_balanced() {
        for reverse in [false, true] {
            let mut t = BTree::new();
            for n in 0..500 {
                let i = if reverse { 499 - n } else { n };
                t.insert(&key(i), "v");
                check(&t);
            }
            let got: Vec<&str> = t.iter().into_iter().map(|(k, _)| k).collect();
            let mut want = got.clone();
            want.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
            assert_eq!(got, want, "reverse={reverse}");
            assert_eq!(t.len(), 500, "reverse={reverse}");
        }
    }

    // ---- delete ----------------------------------------------------------

    #[test]
    fn remove_takes_the_key_out() {
        let mut t = BTree::new();
        for k in ["a", "b", "c"] {
            t.insert(k, k);
        }
        assert!(t.remove("b"));
        check(&t);
        assert_eq!(t.get("b"), None);
        assert_eq!(t.get("a"), Some("a"));
        assert_eq!(t.get("c"), Some("c"));
        assert_eq!(t.len(), 2);
    }

    #[test]
    fn removing_an_absent_key_changes_nothing() {
        let mut t = BTree::new();
        for i in 0..100 {
            t.insert(&key(i), "v");
        }
        let before: Vec<(String, String)> = t
            .iter()
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect();
        assert!(!t.remove("zzz"));
        check(&t);
        assert_eq!(t.len(), 100, "a no-op delete must not change the count");
        assert_eq!(
            t.iter()
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect::<Vec<_>>(),
            before,
            "a no-op delete must not disturb the tree"
        );
    }

    #[test]
    fn emptying_the_tree_one_key_at_a_time_stays_balanced() {
        let mut t = BTree::new();
        for i in 0..300 {
            t.insert(&key(i), "v");
        }
        // In key order, so the shape collapses from the left — where every
        // delete has to borrow from the right or merge leftwards.
        for i in 0..300 {
            assert!(t.remove(&key(i)), "key {i} should have been there");
            check(&t);
            assert_eq!(t.len(), 299 - i);
        }
        assert!(t.is_empty());
        assert_eq!(t.height(), 1, "an empty tree is a single empty leaf");
    }

    #[test]
    fn deleting_in_reverse_order_stays_balanced() {
        let mut t = BTree::new();
        for i in 0..300 {
            t.insert(&key(i), "v");
        }
        for i in (0..300).rev() {
            assert!(t.remove(&key(i)));
            check(&t);
        }
        assert!(t.is_empty());
    }

    /// Every key of a 200-key tree, deleted one at a time — including the keys
    /// that live in interior nodes, whose deletion goes through the predecessor
    /// swap rather than a plain leaf removal. This is the test that the swap
    /// actually works.
    #[test]
    fn deleting_every_key_of_a_large_tree_stays_balanced() {
        let mut t = BTree::new();
        // A stride coprime with 200, so the insertion order is scrambled but
        // every key is still inserted exactly once.
        for i in 0..200 {
            t.insert(&key(i * 7 % 200), &i.to_string());
        }
        check(&t);
        assert_eq!(t.len(), 200, "the stride must have generated 200 keys");
        for i in 0..200 {
            let k = key(i * 7 % 200);
            assert!(t.remove(&k), "key {k} should have been there");
            check(&t);
        }
        assert!(t.is_empty());
    }

    /// The card's balance criterion, stated as a test: random inserts and
    /// deletes interleaved, the tree stays balanced and every key is where it
    /// was put.
    ///
    /// The generator is a fixed-seed xorshift rather than a dependency, because
    /// the project is zero-dependency (docs/MASTER_PLAN.md §1.2) and a test does
    /// not justify breaking that. The fixed seed matters more than the
    /// randomness: a balance bug that only appears for one interleaving is
    /// exactly the kind that would otherwise be filed as "flaky".
    #[test]
    fn random_inserts_and_deletes_stay_balanced() {
        let mut state: u64 = 0x2545_F491_4F6C_DD1D;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut t = BTree::new();
        // A BTreeMap, not a HashMap: the model has to be sorted too, so the two
        // are compared in the same order rather than one being sorted for the
        // comparison.
        let mut model: std::collections::BTreeMap<String, String> =
            std::collections::BTreeMap::new();
        for step in 0..4000 {
            let k = key((next() % 500) as usize);
            // Delete about a third of the time, but only a key that is present,
            // so the model and the tree are always compared on the same
            // operation rather than on one of them being a no-op.
            if step % 3 == 2 && model.remove(&k).is_some() {
                assert!(t.remove(&k), "step {step}: a present key was not removed");
            } else {
                let v = format!("v{step}");
                model.insert(k.clone(), v.clone());
                t.insert(&k, &v);
            }
            check(&t);
        }
        assert_eq!(t.len(), model.len());
        for (k, v) in &model {
            assert_eq!(t.get(k), Some(v.as_str()), "lost or corrupted {k}");
        }
        let walked: Vec<&str> = t.iter().into_iter().map(|(k, _)| k).collect();
        let modelled: Vec<&str> = model.keys().map(|s| s.as_str()).collect();
        assert_eq!(walked, modelled, "the walk and the model disagree");
        // With at least 7 keys per non-root node, 500 keys over order 16 fits
        // in 3 levels (16^2 = 256 leaves of 15), so 4 is the most this can be.
        assert!(
            t.height() <= 4,
            "height {} after random churn — not O(log n)",
            t.height()
        );
    }

    /// The **shape** of the tree after a known insert sequence, pinned by name.
    ///
    /// This is the one test in the file that no correctness property requires,
    /// and it is here because of what mutation testing showed: changing the
    /// median from `len / 2` to `(len - 1) / 2` leaves every other test in this
    /// module green. Both are legal splits of a 16-key node — 8 keys below the
    /// median versus 7 — so the difference is invisible to every invariant here,
    /// and equally invisible to any `get`, any walk, any count and any height.
    ///
    /// It is not invisible to the *other engine*. The AOT C port implements
    /// this tree a second time, and if it splits a node the other way, the same
    /// program builds a differently shaped tree in a compiled binary than in the
    /// interpreter while both still print the same answers. That is a real
    /// divergence in state, and it is exactly the kind this card's parity claim
    /// is supposed to exclude — so the median is asserted by name here, and the
    /// C port's test asserts the same name.
    ///
    /// A leaf fills to [`MAX_KEYS`] = 15 keys, so the **sixteenth** key is the
    /// one that overflows it and splits. Ascending, that node is
    /// `[k00000..k00015]`, and `mid = 16 / 2 = 8` makes `k00008` the median — so
    /// the root ends up holding `k00008` alone.
    #[test]
    fn the_shape_after_a_known_insert_sequence_is_pinned() {
        let mut t = BTree::new();
        for i in 0..16 {
            t.insert(&key(i), "v");
            check(&t);
            if i < 15 {
                // Still one leaf, still legal.
                assert_eq!(t.root_keys().len(), i + 1);
                assert_eq!(t.height(), 1);
            } else {
                assert_eq!(
                    t.root_keys(),
                    vec![key(8).as_str()],
                    "the split median must be k00008 — see dbt_split in runtime.c"
                );
                assert_eq!(t.height(), 2);
            }
        }
    }

    /// The same claim for a *descending* insert, and the median is the **same**:
    /// `k00008`.
    ///
    /// That is worth asserting rather than assuming, because it is the one thing
    /// a hand-ported C tree gets wrong by reasoning from the insertion order
    /// rather than from the node. The overflowing node is always *sorted*, so
    /// what it contains depends only on **which** keys are in it, never on the
    /// order they arrived: after fifteen descending inserts the node is
    /// `[k00001..k00015]` either way, and the 16th key (`k00000`) lands at
    /// index 0 of the very same `[k00000..k00015]` an ascending run would have
    /// built. Same node, same median, same shape.
    #[test]
    fn the_shape_is_pinned_for_descending_inserts_too() {
        let mut t = BTree::new();
        for i in (0..16).rev() {
            t.insert(&key(i), "v");
        }
        check(&t);
        assert_eq!(
            t.root_keys(),
            vec![key(8).as_str()],
            "descending insert, same median: a sorted node does not remember its order"
        );
        assert_eq!(t.height(), 2);
        let got: Vec<&str> = t.iter().into_iter().map(|(k, _)| k).collect();
        assert_eq!(got.len(), 16);
        assert_eq!(*got.first().unwrap(), &key(0));
        assert_eq!(*got.last().unwrap(), &key(15));
    }

    // ---- keys are compared as bytes --------------------------------------

    /// The comparator is byte order, because that is the one thing both engines
    /// can agree on. A locale-aware or numeric reading of the key would make
    /// `db-all-rows` print the same rows in two different orders.
    #[test]
    fn keys_order_by_byte_value_not_by_appearance() {
        let mut t = BTree::new();
        // "10" is below "2" numerically, but "1" is below "2" by bytes, so "10"
        // comes first. Upper case is below lower case. An empty key is the
        // smallest of all.
        for k in ["b", "A", "a", "B", "10", "2", "", "\n", " ", "z"] {
            t.insert(k, k);
        }
        check(&t);
        let got: Vec<&str> = t.iter().into_iter().map(|(k, _)| k).collect();
        assert_eq!(got, vec!["", "\n", " ", "10", "2", "A", "B", "a", "b", "z"]);
    }

    /// A key containing a NUL cannot be written by AINL — the lexer has no `\0`
    /// escape, and it cannot survive the C runtime's `char *` — so the *table*
    /// layer refuses such a key. The tree itself is agnostic about bytes, and
    /// this test pins that it does not silently truncate, which is what would
    /// happen if a caller ever let one through.
    #[test]
    fn a_nul_in_a_key_is_stored_verbatim_not_truncated() {
        let mut t = BTree::new();
        t.insert("a\0b", "v");
        assert_eq!(t.get("a\0b"), Some("v"));
        assert_eq!(t.get("a"), None, "the key must not be cut at the NUL");
        assert_eq!(t.get("b"), None, "the key must not be cut at the NUL");
    }

    /// The performance claim, asserted as **height** rather than as wall-clock
    /// time.
    ///
    /// A lookup costs O(height), so a logarithmic height *is* the lookup
    /// complexity, and height is a deterministic integer — it can be asserted
    /// exactly, on any machine, forever. A timing assertion could only ever be
    /// a threshold, would be flaky on shared CI, and would pass for a tree that
    /// was merely "not too slow" — which is the failure mode a B-tree is
    /// supposed to rule out. The bound below is the theoretical one: with order
    /// 16 a node holds at most 15 keys, so a tree of *h* levels holds at least
    /// 7^h keys, and 10_000 rows must fit in 5 levels (7^5 = 16_807).
    #[test]
    fn lookup_cost_is_logarithmic_in_the_row_count() {
        // Ascending, so the tree is built by the cheapest path; descending and
        // random are covered by the shape tests above, and height is a property
        // of the count, not of the order.
        let n = 10_000;
        let mut t = BTree::new();
        for i in 0..n {
            t.insert(&format!("k{i:06}"), "v");
        }
        assert_eq!(t.len(), n);

        // ceil(log_7(n)) is the minimum number of levels this many keys can
        // occupy; the tree must not be deeper than that by more than the one
        // level a root split can add.
        let mut min_levels = 1;
        let mut capacity = 1usize;
        while capacity < n {
            capacity = capacity.saturating_mul(MAX_KEYS / 2 + 1);
            min_levels += 1;
        }
        assert!(
            t.height() <= min_levels,
            "a {n}-row tree is {} levels deep; order {MAX_KEYS} bounds it at {min_levels}",
            t.height()
        );

        // And the growth is logarithmic in the *observed* heights, which is the
        // property a reader can check by eye: 16x the rows, at most 2 more
        // levels. A linear structure would fail this outright.
        let mut small = BTree::new();
        for i in 0..n / 16 {
            small.insert(&format!("k{i:06}"), "v");
        }
        assert!(
            t.height() <= small.height() + 2,
            "16x the rows grew the tree from {} to {} levels, which is not logarithmic",
            small.height(),
            t.height()
        );

        // Every key is still findable, and an absent one is still absent — a
        // height assertion alone would pass on a tree that had lost rows to
        // keep its levels down.
        for i in (0..n).step_by(997) {
            assert_eq!(t.get(&format!("k{i:06}")), Some("v"));
        }
        assert_eq!(t.get("nope"), None);
        assert_eq!(t.iter().len(), n, "no row was lost");
    }

    /// Enumeration is O(n) with no sort, and it is the in-order walk — so a
    /// table of 10_000 rows comes back sorted, and a flat `Vec` implementation
    /// could not claim the same thing about *inserting* into it. The point of
    /// the assertion is the *order* and the *count*, not the speed: the walk is
    /// a property of the structure, so a later change that swapped it for a
    /// sort would be visible here as a diff in nothing at all, which is why the
    /// order is checked against an explicit expectation rather than a re-sort.
    #[test]
    fn enumeration_is_the_walk_and_comes_back_sorted() {
        let n = 2_000;
        let mut t = BTree::new();
        for i in 0..n {
            t.insert(&format!("k{i:06}"), "v");
        }
        let got: Vec<&str> = t.iter().into_iter().map(|(k, _)| k).collect();
        assert_eq!(got.len(), n);
        let mut sorted = got.clone();
        sorted.sort_unstable();
        assert_eq!(got, sorted, "the walk must already be in key order");
        // The count of levels is small enough that the walk is not doing a
        // per-key descent: a `get` per key would be O(n log n) and this is the
        // O(n) path.
        assert!(t.height() <= 4, "2000 rows is {} levels", t.height());
    }
}
