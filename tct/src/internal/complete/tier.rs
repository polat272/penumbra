use crate::prelude::*;

type N<Child> = super::super::complete::Node<Child>;
type L<Item> = super::super::complete::Leaf<Item>;

/// An eight-deep complete tree with the given item at each leaf.
pub type Nested<Item> = N<N<N<N<N<N<N<N<L<Item>>>>>>>>>;
// Count the levels:    1 2 3 4 5 6 7 8

/// A complete tier of the tiered commitment tree, being an 8-deep sparse quad-tree.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Tier<Item: GetHash + Height> {
    pub(in super::super) inner: Nested<Item>,
}

impl<Item: GetHash + Height> Height for Tier<Item> {
    type Height = <Nested<Item> as Height>::Height;
}

impl<Item: GetHash + Height> GetHash for Tier<Item> {
    #[inline]
    fn hash(&self) -> Hash {
        self.inner.hash()
    }

    #[inline]
    fn cached_hash(&self) -> Option<Hash> {
        self.inner.cached_hash()
    }
}

impl<Item: Complete> Complete for Tier<Item> {
    type Focus = frontier::Tier<Item::Focus>;
}

impl<Item: GetHash + Witness> Witness for Tier<Item> {
    #[inline]
    fn witness(&self, index: impl Into<u64>) -> Option<(AuthPath<Self>, Hash)> {
        self.inner.witness(index)
    }
}

impl<Item: GetHash + ForgetOwned> ForgetOwned for Tier<Item> {
    fn forget_owned(
        self,
        forgotten: Option<Forgotten>,
        index: impl Into<u64>,
    ) -> (Insert<Self>, bool) {
        let (inner, forgotten) = self.inner.forget_owned(forgotten, index);
        (inner.map(|inner| Tier { inner }), forgotten)
    }
}

impl<Item: Complete> From<frontier::Tier<Item::Focus>> for Insert<Tier<Item>> {
    fn from(frontier: frontier::Tier<Item::Focus>) -> Self {
        frontier.finalize_owned()
    }
}

impl<Item: GetHash + Height> GetPosition for Tier<Item> {
    fn position(&self) -> Option<u64> {
        None
    }
}

impl<Item: Height + structure::Any> structure::Any for Tier<Item> {
    fn kind(&self) -> Kind {
        self.inner.kind()
    }

    fn global_position(&self) -> Option<Position> {
        <Self as GetPosition>::position(self).map(Into::into)
    }

    fn forgotten(&self) -> Forgotten {
        structure::Any::forgotten(&self.inner)
    }

    fn children(&self) -> Vec<Node> {
        (&self.inner as &dyn structure::Any).children()
    }
}

impl<Item: GetHash + Height + OutOfOrderOwned> OutOfOrderOwned for Tier<Item> {
    fn uninitialized_out_of_order_insert_commitment_owned(
        this: Insert<Self>,
        index: u64,
        commitment: Commitment,
    ) -> Self {
        Tier {
            inner: Nested::uninitialized_out_of_order_insert_commitment_owned(
                this.map(|tier| tier.inner),
                index,
                commitment,
            ),
        }
    }
}

impl<Item: GetHash + UncheckedSetHash> UncheckedSetHash for Tier<Item> {
    fn unchecked_set_hash(&mut self, index: u64, height: u8, hash: Hash) {
        self.inner.unchecked_set_hash(index, height, hash)
    }

    fn finish_initialize(&mut self) {
        self.inner.finish_initialize()
    }
}
