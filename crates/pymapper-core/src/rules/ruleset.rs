//! Fixed-size bit set over the code space `PM000`..`PM999`.

use std::fmt;

use super::{CODE_SPACE, Rule};

const WORDS: usize = CODE_SPACE.div_ceil(64);

/// A set of rule codes: 128 bytes, `Copy`, O(1) membership.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct RuleSet {
    words: [u64; WORDS],
}

impl RuleSet {
    /// The empty set.
    pub const EMPTY: RuleSet = RuleSet { words: [0; WORDS] };

    /// Adds a rule.
    #[inline]
    pub fn insert(&mut self, rule: Rule) {
        let c = usize::from(rule.0);
        debug_assert!(c < CODE_SPACE, "rule code out of range: {c}");
        self.words[c >> 6] |= 1 << (c & 63);
    }

    /// Removes a rule.
    #[inline]
    pub fn remove(&mut self, rule: Rule) {
        let c = usize::from(rule.0);
        debug_assert!(c < CODE_SPACE, "rule code out of range: {c}");
        self.words[c >> 6] &= !(1 << (c & 63));
    }

    /// Whether the rule is in the set (codes outside the code space never are).
    #[inline]
    pub fn contains(&self, rule: Rule) -> bool {
        let c = usize::from(rule.0);
        c < CODE_SPACE && self.words[c >> 6] & (1 << (c & 63)) != 0
    }

    /// Set union.
    #[inline]
    pub fn union(&self, other: &RuleSet) -> RuleSet {
        let mut out = *self;
        for (a, b) in out.words.iter_mut().zip(&other.words) {
            *a |= b;
        }
        out
    }

    /// Set intersection.
    #[inline]
    pub fn intersection(&self, other: &RuleSet) -> RuleSet {
        let mut out = *self;
        for (a, b) in out.words.iter_mut().zip(&other.words) {
            *a &= b;
        }
        out
    }

    /// Set difference (`self` without `other`).
    #[inline]
    pub fn difference(&self, other: &RuleSet) -> RuleSet {
        let mut out = *self;
        for (a, b) in out.words.iter_mut().zip(&other.words) {
            *a &= !b;
        }
        out
    }

    /// Whether no rule is in the set.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.words.iter().all(|&w| w == 0)
    }

    /// Number of rules in the set.
    pub fn len(&self) -> usize {
        self.words.iter().map(|w| w.count_ones() as usize).sum()
    }

    /// Members in ascending code order. The iterator owns a copy of the set.
    pub fn iter(&self) -> Iter {
        Iter { words: self.words, word: 0 }
    }
}

impl FromIterator<Rule> for RuleSet {
    fn from_iter<I: IntoIterator<Item = Rule>>(iter: I) -> RuleSet {
        let mut set = RuleSet::EMPTY;
        for r in iter {
            set.insert(r);
        }
        set
    }
}

impl fmt::Debug for RuleSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.iter().map(|r| format_args!("{r}").to_string())).finish()
    }
}

/// Iterator over the members of a [`RuleSet`], ascending.
#[derive(Clone, Debug)]
pub struct Iter {
    /// Remaining bits; consumed bits are cleared.
    words: [u64; WORDS],
    word: usize,
}

impl Iterator for Iter {
    type Item = Rule;

    #[inline]
    fn next(&mut self) -> Option<Rule> {
        while self.word < WORDS {
            let w = self.words[self.word];
            if w != 0 {
                self.words[self.word] = w & (w - 1);
                return Some(Rule((self.word * 64 + w.trailing_zeros() as usize) as u16));
            }
            self.word += 1;
        }
        None
    }
}

impl IntoIterator for RuleSet {
    type Item = Rule;
    type IntoIter = Iter;

    fn into_iter(self) -> Iter {
        self.iter()
    }
}
