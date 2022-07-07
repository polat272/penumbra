use std::fmt::Debug;

use serde::{Deserialize, Serialize};

use crate::prelude::*;

/// A frontier of a tier of the tiered commitment tree, being an 8-deep quad-tree of items.
#[derive(Derivative, Serialize, Deserialize)]
#[derivative(
    Debug(bound = "Item: Debug, Item::Complete: Debug"),
    Clone(bound = "Item: Clone, Item::Complete: Clone")
)]
#[serde(bound(
    serialize = "Item: Serialize, Item::Complete: Serialize",
    deserialize = "Item: Deserialize<'de>, Item::Complete: Deserialize<'de>"
))]
pub struct Tier<Item: Focus> {
    inner: Inner<Item>,
}

type N<Focus> = frontier::Node<Focus>;
type L<Item> = frontier::Leaf<Item>;

/// An eight-deep frontier tree with the given item stored in each leaf.
pub type Nested<Item> = N<N<N<N<N<N<N<N<L<Item>>>>>>>>>;
// Count the levels:    1 2 3 4 5 6 7 8

/// The inside of a frontier of a tier.
#[derive(Derivative, Serialize, Deserialize)]
#[derivative(
    Debug(bound = "Item: Debug, Item::Complete: Debug"),
    Clone(bound = "Item: Clone, Item::Complete: Clone")
)]
#[serde(bound(
    serialize = "Item: Serialize, Item::Complete: Serialize",
    deserialize = "Item: Deserialize<'de>, Item::Complete: Deserialize<'de>"
))]
pub enum Inner<Item: Focus> {
    /// A tree with at least one leaf.
    Frontier(Box<Nested<Item>>),
    /// A completed tree which has at least one witnessed child.
    Complete(complete::Nested<Item::Complete>),
    /// A tree which has been filled, but which witnessed no elements, so it is represented by a
    /// single hash.
    Hash(Hash),
}

impl<Item: Focus> From<Hash> for Tier<Item> {
    #[inline]
    fn from(hash: Hash) -> Self {
        Self {
            inner: Inner::Hash(hash),
        }
    }
}

impl<Item: Focus> Tier<Item> {
    /// Create a new tier from a single item which will be its first element.
    #[inline]
    pub fn new(item: Item) -> Self {
        Self {
            inner: Inner::Frontier(Box::new(Nested::new(item))),
        }
    }

    /// Insert an item or its hash into this frontier tier.
    ///
    /// If the tier is full, return the input item without inserting it.
    #[inline]
    pub fn insert(&mut self, item: Item) -> Result<(), Item> {
        // Temporarily swap the inside for the empty hash (this will get put back immediately)
        let inner = std::mem::replace(&mut self.inner, Inner::Hash(Hash::zero()));

        let result;
        (result, *self) = match (Self { inner }.insert_owned(item)) {
            Ok(this) => (Ok(()), this),
            Err((item, this)) => (Err(item), this),
        };

        result
    }

    #[inline]
    fn insert_owned(self, item: Item) -> Result<Self, (Item, Self)> {
        match self.inner {
            // The tier is full or is a single hash, so return the item without inserting it
            inner @ (Inner::Complete(_) | Inner::Hash(_)) => Err((item, Self { inner })),
            // The tier is a frontier, so try inserting into it
            Inner::Frontier(frontier) => {
                if frontier.is_full() {
                    // Don't even try inserting when we know it will fail: this means that there is *no
                    // implicit finalization* of the frontier, even when it is full
                    Err((
                        item,
                        Self {
                            inner: Inner::Frontier(frontier),
                        },
                    ))
                } else {
                    // If it's not full, then insert the item into it (which we know will succeed)
                    let inner =
                        Inner::Frontier(Box::new(frontier.insert_owned(item).unwrap_or_else(
                            |_| panic!("frontier is not full, so insert must succeed"),
                        )));
                    Ok(Self { inner })
                }
            }
        }
    }

    /// Update the focused element of this tier using a function.
    ///
    /// If the tier is empty or finalized, the function is not executed, and this returns `None`.
    #[inline]
    pub fn update<T>(&mut self, f: impl FnOnce(&mut Item) -> T) -> Option<T> {
        if let Inner::Frontier(frontier) = &mut self.inner {
            frontier.update(f)
        } else {
            None
        }
    }

    /// Get the focused element of this tier, if one exists.
    ///
    /// If the tier is empty or finalized, returns `None`.
    #[inline]
    pub fn focus(&self) -> Option<&Item> {
        if let Inner::Frontier(frontier) = &self.inner {
            frontier.focus()
        } else {
            None
        }
    }

    /// Check whether this tier is full.
    ///
    /// If this returns `false`, then insertion will fail.
    #[inline]
    pub fn is_full(&self) -> bool {
        match &self.inner {
            Inner::Frontier(frontier) => frontier.is_full(),
            Inner::Complete(_) | Inner::Hash(_) => true,
        }
    }

    /// Finalize this tier so that it is internally marked as complete.
    #[inline]
    pub fn finalize(&mut self) -> bool {
        let already_finalized = self.is_finalized();

        // Temporarily replace the inside with the zero hash (it will get put back right away, this
        // is just to satisfy the borrow checker)
        let inner = std::mem::replace(&mut self.inner, Inner::Hash(Hash::zero()));

        self.inner = match inner {
            Inner::Frontier(frontier) => match frontier.finalize_owned() {
                Insert::Hash(hash) => Inner::Hash(hash),
                Insert::Keep(inner) => Inner::Complete(inner),
            },
            inner @ (Inner::Complete(_) | Inner::Hash(_)) => inner,
        };

        already_finalized
    }

    /// Check whether this tier has been finalized.
    #[inline]
    pub fn is_finalized(&self) -> bool {
        match self.inner {
            Inner::Frontier(_) => false,
            Inner::Complete(_) | Inner::Hash(_) => true,
        }
    }
}

impl<Item: Focus> Height for Tier<Item> {
    type Height = <Nested<Item> as Height>::Height;
}

impl<Item: Focus> GetHash for Tier<Item> {
    #[inline]
    fn hash(&self) -> Hash {
        match &self.inner {
            Inner::Frontier(frontier) => frontier.hash(),
            Inner::Complete(complete) => complete.hash(),
            Inner::Hash(hash) => *hash,
        }
    }

    #[inline]
    fn cached_hash(&self) -> Option<Hash> {
        match &self.inner {
            Inner::Frontier(frontier) => frontier.cached_hash(),
            Inner::Complete(complete) => complete.cached_hash(),
            Inner::Hash(hash) => Some(*hash),
        }
    }
}

impl<Item: Focus> Focus for Tier<Item> {
    type Complete = complete::Tier<Item::Complete>;

    #[inline]
    fn finalize_owned(self) -> Insert<Self::Complete> {
        match self.inner {
            Inner::Frontier(frontier) => match frontier.finalize_owned() {
                Insert::Hash(hash) => Insert::Hash(hash),
                Insert::Keep(inner) => Insert::Keep(complete::Tier { inner }),
            },
            Inner::Complete(inner) => Insert::Keep(complete::Tier { inner }),
            Inner::Hash(hash) => Insert::Hash(hash),
        }
    }
}

impl<Item: Focus + Witness> Witness for Tier<Item>
where
    Item::Complete: Witness,
{
    #[inline]
    fn witness(&self, index: impl Into<u64>) -> Option<(AuthPath<Self>, Hash)> {
        match &self.inner {
            Inner::Frontier(frontier) => frontier.witness(index),
            Inner::Complete(complete) => complete.witness(index),
            Inner::Hash(_) => None,
        }
    }
}

impl<Item: Focus + GetPosition> GetPosition for Tier<Item> {
    #[inline]
    fn position(&self) -> Option<u64> {
        match &self.inner {
            Inner::Frontier(frontier) => frontier.position(),
            Inner::Complete(_) | Inner::Hash(_) => None,
        }
    }
}

impl<Item: Focus + Forget> Forget for Tier<Item>
where
    Item::Complete: ForgetOwned,
{
    #[inline]
    fn forget(&mut self, forgotten: Option<Forgotten>, index: impl Into<u64>) -> bool {
        // Whether something was actually forgotten
        let was_forgotten;

        // Temporarily replace the inside with the zero hash (it will get put back right away, this
        // is just to satisfy the borrow checker)
        let inner = std::mem::replace(&mut self.inner, Inner::Hash(Hash::zero()));

        (was_forgotten, self.inner) = match inner {
            // If the tier is a frontier, try to forget from the frontier path, if it's not empty
            Inner::Frontier(mut frontier) => {
                (frontier.forget(forgotten, index), Inner::Frontier(frontier))
            }
            // If the tier is complete, forget from the complete tier and if it resulted in a hash,
            // set the self to that hash
            Inner::Complete(complete) => match complete.forget_owned(forgotten, index) {
                (Insert::Keep(complete), forgotten) => (forgotten, Inner::Complete(complete)),
                (Insert::Hash(hash), forgotten) => (forgotten, Inner::Hash(hash)),
            },
            // If the tier was just a hash, nothing to do
            Inner::Hash(hash) => (false, Inner::Hash(hash)),
        };

        // Return whether something was actually forgotten
        was_forgotten
    }
}

impl<Item: Focus> From<complete::Tier<Item::Complete>> for Tier<Item> {
    fn from(complete: complete::Tier<Item::Complete>) -> Self {
        Self {
            inner: Inner::Complete(complete.inner),
        }
    }
}

impl<Item: Focus> From<complete::Top<Item::Complete>> for Tier<Item> {
    fn from(complete: complete::Top<Item::Complete>) -> Self {
        Self {
            inner: Inner::Complete(complete.inner),
        }
    }
}

impl<Item: Focus + GetPosition + Height + structure::Any> structure::Any for Tier<Item>
where
    Item::Complete: structure::Any,
{
    fn kind(&self) -> Kind {
        Kind::Internal {
            height: <Self as Height>::Height::HEIGHT,
        }
    }

    fn global_position(&self) -> Option<Position> {
        <Self as GetPosition>::position(self).map(Into::into)
    }

    fn forgotten(&self) -> Forgotten {
        match &self.inner {
            Inner::Frontier(frontier) => (&**frontier as &dyn structure::Any).forgotten(),
            Inner::Complete(complete) => (complete as &dyn structure::Any).forgotten(),
            Inner::Hash(_) => Forgotten::default(),
        }
    }

    fn children(&self) -> Vec<structure::Node> {
        match &self.inner {
            Inner::Frontier(frontier) => frontier.children(),
            Inner::Complete(complete) => (complete as &dyn structure::Any).children(),
            Inner::Hash(_) => vec![],
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn check_inner_size() {
        static_assertions::assert_eq_size!(Tier<Tier<Tier<frontier::Item>>>, [u8; 88]);
    }

    #[test]
    fn position_advances_by_one() {
        let mut tier: Tier<Item> = Tier::new(Hash::zero().into());
        for expected_position in 1..=(u16::MAX as u64) {
            assert_eq!(tier.position(), Some(expected_position));
            tier.insert(Hash::zero().into()).unwrap();
        }
        assert_eq!(tier.position(), None);
    }
}
