#![allow(clippy::unusual_byte_groupings)]

//! Non-incremental deserialization for the [`Tree`](crate::Tree).

use futures::StreamExt;
use hash_hasher::HashedMap;

use crate::prelude::*;
use crate::storage::Read;

/// Deserialize a [`Tree`] from a storage backend.
pub async fn from_reader<R: Read>(reader: &mut R) -> Result<Tree, R::Error> {
    // Make an uninitialized tree with the correct position
    let mut inner: frontier::Top<frontier::Tier<frontier::Tier<frontier::Item>>> =
        OutOfOrder::uninitialized(reader.position().await?.map(Into::into));

    // Make an index to track the commitments (we'll assemble this into the final tree)
    let mut index = HashedMap::default();

    // Insert all the commitments into the tree, simultaneously building the index
    let mut commitments = reader.commitments();
    while let Some((position, commitment)) = commitments.next().await.transpose()? {
        inner.uninitialized_out_of_order_insert_commitment(position.into(), commitment);
        index.insert(commitment, u64::from(position).into());
    }

    drop(commitments); // explicit drop to satisfy borrow checker

    // Set all the hashes in the tree
    let mut hashes = reader.hashes();
    while let Some((position, height, hash)) = hashes.next().await.transpose()? {
        inner.unchecked_set_hash(position.into(), height, hash);
    }

    // Finalize the tree by recomputing all missing hashes
    inner.finish_initialize();

    Ok(Tree::unchecked_from_parts(index, inner))
}
