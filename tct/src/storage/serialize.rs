//! Incremental serialization for the [`Tree`](crate::Tree).

use decaf377::FieldExt;
use futures::{Stream, StreamExt};
use poseidon377::Fq;
use serde::de::Visitor;
use std::pin::Pin;

use crate::prelude::*;
use crate::storage::Write;
use crate::structure::{Kind, Place};
use crate::tree::Position;

pub(crate) mod fq;

/// Options for serializing a tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Serializer {
    /// The options for the serialization.
    options: Options,
    /// The minimum position of node which should be included in the serialization.
    ///
    /// If this is `None` then the minimum position does not exist, i.e. the tree is totally full.
    minimum_position: Option<Position>,
    /// The minimum forgotten version which should be reported for deletion.
    last_forgotten: Forgotten,
}

impl Serializer {
    fn should_keep_hash(&self, node: &structure::Node, children: usize) -> bool {
        // A node's hash is recalculable if it has children or if it has a witnessed commitment
        let is_recalculable = children > 0
            || matches!(
                node.kind(),
                Kind::Leaf {
                    commitment: Some(_)
                }
            );
        // A node's hash is essential (cannot be recalculated from other information) if it is not
        // recalculable
        let is_essential = !is_recalculable;
        // A node is on the frontier if its place matches `Place::Frontier`
        let is_frontier = matches!(node.place(), Place::Frontier);
        // A node is complete if it's not on the frontier
        let is_complete = !is_frontier;

        is_essential || (is_complete && self.options.keep_internal)
    }

    fn should_keep_children(&self, node: &structure::Node) -> bool {
        if let Some(minimum_position) = self.minimum_position {
            node.range().contains(&minimum_position)
        } else {
            // If the minimum position in the serializer is not specified, then that means we should
            // just abort serialization of internal commitments and hashes, because we've already
            // serialized all we will ever need to
            false
        }
    }

    /// Create a new default serializer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the minimum position to include in the serialization.
    pub fn position(&mut self, position: Option<Position>) -> &mut Self {
        self.minimum_position = position;
        self
    }

    /// Set the last forgotten version to include in the serialization of forgettable locations.
    pub fn last_forgotten(&mut self, forgotten: Forgotten) -> &mut Self {
        self.last_forgotten = forgotten;
        self
    }

    /// Set the serializer to keep internal complete hashes in the output (this is the default).
    ///
    /// If complete internal hashes are kept, this significantly reduces the amount of computation
    /// upon deserialization, since if they are not cached, a number of hashes proportionate to the
    /// number of witnessed commitments need to be recomputed. However, this also imposes a linear
    /// space overhead on the total amount of serialized data.
    pub fn keep_internal(&mut self) -> &mut Self {
        self.options.keep_internal();
        self
    }

    /// Set the serializer to omit internal complete hashes in the output.
    ///
    /// If complete internal hashes are kept, this significantly reduces the amount of computation
    /// upon deserialization, since if they are not cached, a number of hashes proportionate to the
    /// number of witnessed commitments need to be recomputed. However, this also imposes a linear
    /// space overhead on the total amount of serialized data.
    pub fn omit_internal(&mut self) -> &mut Self {
        self.options.omit_internal();
        self
    }

    /// Serialize a tree's structure into a depth-first pre-order traversal of hashes within it.
    pub fn hashes_stream<'tree>(
        &self,
        tree: &'tree crate::Tree,
    ) -> impl Stream<Item = (Position, u8, Hash)> + Unpin + 'tree {
        fn hashes_inner(
            options: Serializer,
            node: structure::Node,
        ) -> Pin<Box<dyn Stream<Item = (Position, u8, Hash)> + '_>> {
            Box::pin(stream! {
                let position = node.position();
                let height = node.height();
                let children = node.children();

                // If the minimum position is too high, then don't keep this node (but maybe some of
                // its children will be kept)
                if u64::from(position)
                    // If the minimum position is `None`, then we never consider this as a possible
                    // hash to emit, because the stored position already contains every
                    // possible hash it ever will
                    >= options.minimum_position.map(Into::into).unwrap_or(u64::MAX) {
                    if options.should_keep_hash(&node, children.len()) {
                        if let Some(hash) = node.cached_hash() {
                            yield (position, height, hash);
                        }
                    }
                }

                // Traverse the children in order, provided that the minimum position doesn't preclude this
                if options.should_keep_children(&node) {
                    for child in children {
                        let mut stream = hashes_inner(options, child);
                        while let Some(point) = stream.next().await {
                            yield point;
                        }
                    }
                }
            })
        }

        hashes_inner(*self, tree.structure())
    }

    /// Serialize a tree's structure into an iterator of hashes within it, for use in synchronous
    /// contexts.
    pub fn hashes_iter<'tree>(
        &self,
        tree: &'tree crate::Tree,
    ) -> impl Iterator<Item = (Position, u8, Hash)> + 'tree {
        futures::executor::block_on_stream(self.hashes_stream(tree))
    }

    /// Serialize a tree's structure into a depth-first pre-order traversal of hashes within it.
    pub fn commitments_stream<'tree>(
        &self,
        tree: &'tree crate::Tree,
    ) -> impl Stream<Item = (Position, Commitment)> + Unpin + 'tree {
        fn commitments_inner(
            options: Serializer,
            node: structure::Node,
        ) -> Pin<Box<dyn Stream<Item = (Position, Commitment)> + '_>> {
            Box::pin(stream! {
                let position = node.position();
                let children = node.children();

                // If the minimum position is too high, then don't keep this node (but maybe some of
                // its children will be kept)
                if u64::from(position)
                    // If the minimum position is `None`, then we never consider this as a possible
                    // commitment to emit, because the stored position already contains every
                    // possible commitment it ever will
                    >= options.minimum_position.map(Into::into).unwrap_or(u64::MAX)
                {
                    // If we're at a witnessed commitment, yield it
                    if let Kind::Leaf {
                        commitment: Some(commitment),
                    } = node.kind()
                    {
                        yield (position, commitment);
                    }
                }

                // Traverse the children in order, provided that the minimum position doesn't preclude this
                if options.should_keep_children(&node) {
                    for child in children {
                        let mut stream = commitments_inner(options, child);
                        while let Some(point) = stream.next().await {
                            yield point;
                        }
                    }
                }
            })
        }

        commitments_inner(*self, tree.structure())
    }

    /// Serialize a tree's structure into an iterator of commitments within it, for use in
    /// synchronous contexts.
    pub fn commitments_iter<'tree>(
        &self,
        tree: &'tree crate::Tree,
    ) -> impl Iterator<Item = (Position, Commitment)> + 'tree {
        futures::executor::block_on_stream(self.commitments_stream(tree))
    }

    /// Get a stream of forgotten locations, which can be deleted from incremental storage.
    pub fn forgotten_stream<'tree>(
        &self,
        tree: &'tree crate::Tree,
    ) -> impl Stream<Item = (Position, u8, Hash)> + Unpin + 'tree {
        fn forgotten_inner(
            options: Serializer,
            node: structure::Node,
        ) -> Pin<Box<dyn Stream<Item = (Position, u8, Hash)> + '_>> {
            Box::pin(stream! {
                // Only report nodes (and their children) which are less than the minimum position
                // (because those greater will not have yet been serialized to storage) and greater
                // than or equal to the minimum forgotten version (because those lesser will already
                // have been deleted from storage)
                if u64::from(node.position())
                    // If the minimum position is `None`, then we always consider this as
                    // potentially forgotten, since `None` is the maximum position + 1
                    < options.minimum_position.map(Into::into).unwrap_or(u64::MAX)
                    && node.forgotten() > options.last_forgotten
                {
                    let children = node.children();
                    if children.is_empty() {
                        // If there are no children, report the point
                        yield (
                            node.position().into(),
                            node.height(),
                            // A node with no children definitely has a precalculated hash, so this
                            // is not evaluating any extra hashes
                            node.hash().into(),
                        );
                    } else {
                        // If there are children, this node was not yet forgotten, but because the
                        // node's forgotten version is greater than the minimum forgotten specified
                        // in the options, we know there is some child which needs to be accounted for
                        for child in children {
                            let mut stream = forgotten_inner(options, child);
                            while let Some(point) = stream.next().await {
                                yield point;
                            }
                        }
                    }
                }
            })
        }

        forgotten_inner(*self, tree.structure())
    }

    /// Get an iterator of forgotten locations, which can be deleted from incremental storage., for
    /// use in synchronous contexts.
    pub fn forgotten_iter<'tree>(
        &self,
        tree: &'tree crate::Tree,
    ) -> impl Iterator<Item = (Position, u8, Hash)> + 'tree {
        futures::executor::block_on_stream(self.forgotten_stream(tree))
    }
}

/// Options for serializing a tree to a writer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Options {
    /// Should the internal hashes of complete nodes be preserved?
    keep_internal: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            keep_internal: true,
        }
    }
}

impl Options {
    /// Set the serializer to keep internal complete hashes in the output (this is the default).
    ///
    /// If complete internal hashes are kept, this significantly reduces the amount of computation
    /// upon deserialization, since if they are not cached, a number of hashes proportionate to the
    /// number of witnessed commitments need to be recomputed. However, this also imposes a linear
    /// space overhead on the total amount of serialized data.
    pub fn keep_internal(&mut self) -> &mut Self {
        self.keep_internal = true;
        self
    }

    /// Set the serializer to omit internal complete hashes in the output.
    ///
    /// If complete internal hashes are kept, this significantly reduces the amount of computation
    /// upon deserialization, since if they are not cached, a number of hashes proportionate to the
    /// number of witnessed commitments need to be recomputed. However, this also imposes a linear
    /// space overhead on the total amount of serialized data.
    pub fn omit_internal(&mut self) -> &mut Self {
        self.keep_internal = false;
        self
    }
}

/// Serialize the changes to a [`Tree`](crate::Tree) into a writer, deleting all forgotten nodes and
/// adding all new nodes.
pub async fn to_writer<W: Write>(
    options: Options,
    last_forgotten: Forgotten,
    writer: &mut W,
    tree: &crate::Tree,
) -> Result<(), W::Error> {
    // Grab the current position stored in storage
    let minimum_position = writer.position().await?;

    let serializer = Serializer {
        options,
        last_forgotten,
        minimum_position,
    };

    // Write all the new points
    let mut new_hashes = serializer.hashes_stream(tree);
    while let Some((position, height, hash)) = new_hashes.next().await {
        writer.add_hash(position, height, hash).await?;
    }

    // Delete all the forgotten points
    let mut forgotten_points = serializer.forgotten_stream(tree);
    while let Some((position, below_height, _hash)) = forgotten_points.next().await {
        // Calculate the range of positions to delete, based on the height
        let position = u64::from(position);
        let stride = 4u64.pow(below_height.into());
        let range = position.into()..(position + stride).min(4u64.pow(24) - 1).into();

        // Delete the range of positions
        writer.delete_range(below_height, range).await?;
    }

    // Update the position
    writer.set_position(tree.position().map(Into::into)).await?;

    Ok(())
}
