use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

use anyhow::anyhow;
use bytes::Bytes;
use futures::future::FutureExt;
use metrics::increment_counter;
use rand_core::OsRng;
use tendermint::abci::{
    request::{self, BeginBlock, CheckTxKind, EndBlock},
    response, Request, Response,
};
use tokio::sync::oneshot;
use tower::Service;
use tower_abci::BoxError;
use tracing::{instrument, Instrument, Span};

use penumbra_crypto::{
    asset,
    merkle::TreeExt,
    merkle::{self, NoteCommitmentTree},
    note, Nullifier, Transaction,
};

use crate::{
    db::schema,
    genesis::GenesisAppState,
    staking::Validator,
    verify::{mark_genesis_as_verified, StatefulTransactionExt, StatelessTransactionExt},
    PendingBlock, RequestExt, State,
};

const ABCI_INFO_VERSION: &str = env!("VERGEN_GIT_SEMVER");

const NUM_RECENT_ANCHORS: usize = 64;

/// The Penumbra ABCI application.
#[derive(Debug)]
pub struct App {
    state: State,

    /// Written to the database after every block commit.
    note_commitment_tree: merkle::BridgeTree<note::Commitment, { merkle::DEPTH as u8 }>,

    /// Recent anchors of the note commitment tree.
    recent_anchors: VecDeque<merkle::Root>,

    /// We want to prevent two transactions from spending the same note in the
    /// same block.  Our only control over whether transactions will appear in a
    /// block is in `CheckTx`, which gates access to the entire mempool, so we
    /// want to enforce that no two transactions in the mempool spend the same
    /// note.
    ///
    /// To do this, we add a mempool transaction's nullifiers to this set in
    /// `CheckTx` and remove them when we see they've been committed to a block
    /// (in `Commit`).  This means that if Tendermint pulls transactions from
    /// the mempool as part of default block proposer logic, no conflicting
    /// transactions can appear.
    ///
    /// However, it doesn't prevent a malicious validator from proposing
    /// conflicting transactions, so we need to ensure (in `DeliverTx`) that we
    /// ignore invalid transactions.
    mempool_nullifiers: Arc<Mutex<BTreeSet<Nullifier>>>,

    /// Contains all queued state changes for the duration of a block.  This is
    /// set to Some at the beginning of BeginBlock and consumed (and reset to
    /// None) in Commit.
    pending_block: Option<Arc<Mutex<PendingBlock>>>,

    /// Used to allow asynchronous requests to be processed sequentially.
    completion_tracker: CompletionTracker,

    /// Epoch duration in blocks
    epoch_duration: u64,

    /// Contains the validator set, with each validator uniquely identified by their tendermint
    /// public key.
    validators: Arc<Mutex<BTreeMap<tendermint::PublicKey, Validator>>>,
}

impl App {
    /// Create the application with the given DB state.
    #[instrument(skip(state))]
    pub async fn new(state: State) -> Result<Self, anyhow::Error> {
        let note_commitment_tree = state.note_commitment_tree().await?;
        let genesis_config = state.genesis_configuration().await?;
        let recent_anchors = state.recent_anchors(NUM_RECENT_ANCHORS).await?;
        let validators = state.validators().await?;
        Ok(Self {
            state,
            note_commitment_tree,
            recent_anchors: recent_anchors,
            mempool_nullifiers: Arc::new(Default::default()),
            validators: Arc::new(Mutex::new(validators)),
            pending_block: None,
            completion_tracker: Default::default(),
            epoch_duration: genesis_config.epoch_duration,
        })
    }

    fn init_genesis(
        &mut self,
        init_chain: request::InitChain,
    ) -> impl Future<Output = Result<Response, BoxError>> {
        tracing::info!(?init_chain);
        let mut genesis_block = PendingBlock::new(NoteCommitmentTree::new(0));
        genesis_block.set_height(0);

        // Note that errors cannot be handled in InitChain, the application must crash.
        let genesis: GenesisAppState = serde_json::from_slice(&init_chain.app_state_bytes)
            .expect("can parse app_state in genesis file");

        // Create genesis transaction and update database table `transactions`.
        let mut genesis_tx_builder =
            Transaction::genesis_build_with_root(self.note_commitment_tree.root2());

        for note in &genesis.notes {
            tracing::info!(?note);
            genesis_tx_builder.add_output(
                &mut OsRng,
                note::Note::try_from(note).expect("GenesisNote can be converted into regular Note"),
            );
        }

        // xx todo: Warn on genesis notes that do not correspond to any known asset.

        for asset in &genesis.assets {
            // The base is used to derive the asset_id
            tracing::info!(?asset);
            genesis_block
                .new_assets
                .insert(asset::Denom(asset.base.clone()).into(), asset.clone());
        }

        let genesis_tx = genesis_tx_builder
            .set_chain_id(init_chain.chain_id)
            .finalize(&mut OsRng)
            .expect("can form genesis transaction");
        let verified_transaction = mark_genesis_as_verified(genesis_tx);

        // Now add the transaction and its note fragments to the pending state changes.
        genesis_block.add_transaction(verified_transaction);
        tracing::info!("loaded all genesis notes");

        // load the validators from the initial Tendermint genesis state
        let mut validators = BTreeMap::new();
        for validator_update in init_chain.validators.iter() {
            validators.insert(
                validator_update.pub_key,
                Validator::new(validator_update.pub_key, validator_update.power),
            );
        }
        self.validators = Arc::new(Mutex::new(validators.clone()));

        self.epoch_duration = genesis.epoch_duration;

        // construct the pending block and commit the initial state
        self.pending_block = Some(Arc::new(Mutex::new(genesis_block)));
        let commit = self.commit();
        let state = self.state.clone();
        let gc = genesis.clone();
        async move {
            commit.await?;

            // Save the genesis config to the blobs table for future reference.
            state
                .set_genesis_configuration(&gc)
                .await
                .expect("able to save genesis config to blobs table");

            state.set_initial_validators(&validators).await?;
            let app_hash = state.app_hash().await.unwrap();
            Ok(Response::InitChain(response::InitChain {
                consensus_params: Some(init_chain.consensus_params),
                validators: init_chain.validators,
                app_hash: app_hash.into(),
            }))
        }
    }

    fn info(&self) -> impl Future<Output = Result<Response, BoxError>> {
        let state = self.state.clone();
        async move {
            let (last_block_height, last_block_app_hash) = match state.latest_block_info().await? {
                Some(schema::BlocksRow {
                    height, app_hash, ..
                }) => (height.try_into().unwrap(), app_hash.into()),
                None => (0u32.into(), vec![0; 32].into()),
            };

            Ok(Response::Info(response::Info {
                data: "penumbra".to_string(),
                version: ABCI_INFO_VERSION.to_string(),
                app_version: 1,
                last_block_height,
                last_block_app_hash,
            }))
        }
        .instrument(Span::current())
    }

    fn query(&self, _query: Bytes) -> response::Query {
        // TODO: implement (#22)
        Default::default()
    }

    fn begin_block(&mut self, _begin: BeginBlock) -> response::BeginBlock {
        self.pending_block = Some(Arc::new(Mutex::new(PendingBlock::new(
            self.note_commitment_tree.clone(),
        ))));
        // TODO: process begin.last_commit_info to handle validator rewards, and
        // begin.byzantine_validators to handle evidence + slashing
        response::BeginBlock::default()
    }

    /// Perform checks before adding a transaction into the mempool via `CheckTx`.
    ///
    /// In the transaction validation performed before adding a transaction into the
    /// mempool, we check that:
    ///
    /// * All binding and auth sigs signatures verify (stateless),
    /// * All proofs verify (stateless and stateful),
    /// * The transaction does not reveal nullifiers already revealed in another transaction
    /// in the mempool or in the database,
    ///
    /// If a transaction does not pass these checks, we return a non-zero `CheckTx` response
    /// code, and the transaction will not be added into the mempool.
    ///
    /// We do not queue up any state changes into `PendingBlock` until `DeliverTx` where these
    /// checks are repeated.
    fn check_tx(
        &mut self,
        request: request::CheckTx,
    ) -> impl Future<Output = Result<(), anyhow::Error>> {
        let state = self.state.clone();
        let mempool_nullifiers = self.mempool_nullifiers.clone();
        let recent_anchors = self.recent_anchors.clone();

        async move {
            let pending_transaction =
                Transaction::try_from(request.tx.as_ref())?.verify_stateless()?;

            // Ensure we do not add any transactions with duplicate nullifiers into the mempool.
            //
            // Note that we only run this logic if this `CheckTx` request is from a new transaction
            // (i.e. `CheckTxKind::New`). If this is a recheck of an existing entry in the mempool,
            // then we don't need to add the nullifier again, as it's already in `self.mempool_nullifiers`.
            // Rechecks occur whenever a block is committed if the Tendermint `mempool.recheck` option is
            // true, which is the default option.
            if request.kind == CheckTxKind::New {
                for nullifier in pending_transaction.spent_nullifiers.clone() {
                    if mempool_nullifiers.lock().unwrap().contains(&nullifier) {
                        return Err(anyhow!(
                            "nullifer {:?} already present in mempool_nullifiers",
                            nullifier
                        ));
                    } else {
                        mempool_nullifiers.lock().unwrap().insert(nullifier);
                    }
                }
            }

            // Ensure that we do not add any transactions that have spent nullifiers in the database.
            for nullifier in pending_transaction.spent_nullifiers.clone() {
                if state
                    .nullifier(nullifier.clone())
                    .await
                    .expect("must be able to fetch nullifier")
                    .is_some()
                {
                    return Err(anyhow!(
                        "nullifer {:?} already present in database",
                        nullifier
                    ));
                };
            }

            pending_transaction.verify_stateful(&recent_anchors)?;

            Ok(())
        }
    }

    /// Perform full transaction validation via `DeliverTx`.
    ///
    /// State changes are only applied for valid transactions. Invalid transaction are ignored.
    ///
    /// We must perform all checks again here even though they are performed in `CheckTx`, as a
    /// Byzantine node may propose a block containing double spends or other disallowed behavior,
    /// so it is not safe to assume all checks performed in `CheckTx` were done.
    fn deliver_tx(&mut self, txbytes: Bytes) -> impl Future<Output = Result<(), anyhow::Error>> {
        let state = self.state.clone();
        let recent_anchors = self.recent_anchors.clone();
        let pending_block_ref = self.pending_block.clone();

        async move {
            let pending_transaction =
                Transaction::try_from(txbytes.as_ref())?.verify_stateless()?;

            for nullifier in pending_transaction.spent_nullifiers.clone() {
                // verify that we're not spending a nullifier that was already spent in a previous block
                if state
                    .nullifier(nullifier.clone())
                    .await
                    .expect("must be able to fetch nullifier")
                    .is_some()
                {
                    return Err(anyhow!(
                        "nullifer {:?} already present in database",
                        nullifier
                    ));
                };
                // verify that we're not spending a nullifier that was already spent in this block
                if pending_block_ref
                    .as_ref()
                    .expect("pending_block must be Some in DeliverTx")
                    .lock()
                    .unwrap()
                    .spent_nullifiers
                    .contains(&nullifier)
                {
                    return Err(anyhow!(
                        "nullifier {:?} was already spent in this block",
                        nullifier
                    ));
                }
            }

            let verified_transaction = pending_transaction.verify_stateful(&recent_anchors)?;

            // We accumulate data only for `VerifiedTransaction`s into `PendingBlock`.
            pending_block_ref
                .expect("pending_block must be Some in DeliverTx")
                .lock()
                .unwrap()
                .add_transaction(verified_transaction);

            increment_counter!("node_transactions_total");
            Ok(())
        }
    }

    fn end_block(&mut self, end: EndBlock) -> response::EndBlock {
        self.pending_block
            .as_mut()
            .expect("pending_block must be Some in EndBlock")
            .lock()
            .unwrap()
            .set_height(end.height);

        if end.height < 0 {
            panic!("block height should never be negative");
        }

        // TODO: if necessary, set the EndBlock response to add validators
        // at the epoch boundary
        if end.height.unsigned_abs() % self.epoch_duration == 0 {
            // Epoch boundary -- add/remove validators if necessary
            tracing::info!("new epoch");
        }
        // TODO: here's where we process validator changes
        response::EndBlock::default()
    }

    /// Commit the queued state transitions.
    fn commit(&mut self) -> impl Future<Output = Result<Response, BoxError>> {
        let pending_block_ref = self
            .pending_block
            .take()
            .expect("pending_block must be Some in Commit");

        let pending_block = Arc::try_unwrap(pending_block_ref)
            .expect("can't try_unwrap on Arc<Mutex<PendingBlock>>>")
            .into_inner()
            .expect("cannot access inner PendingBlock");

        // These nullifiers are about to be committed, so we don't need
        // to keep them in the mempool nullifier set any longer.
        for nullifier in pending_block.spent_nullifiers.iter() {
            self.mempool_nullifiers.lock().unwrap().remove(nullifier);
            increment_counter!("node_spent_nullifiers_total");
        }

        // Pull the updated note commitment tree.
        self.note_commitment_tree = pending_block.note_commitment_tree.clone();
        let anchor = self.note_commitment_tree.root2();
        self.recent_anchors.push_front(anchor);
        if self.recent_anchors.len() > NUM_RECENT_ANCHORS {
            self.recent_anchors.pop_back();
        }

        let finished_signal = self.completion_tracker.start();
        let state = self.state.clone();
        async move {
            state
                .commit_block(pending_block)
                .await
                .expect("block commit should succeed");

            let app_hash = state
                .app_hash()
                .await
                .expect("must be able to fetch apphash");

            // Signal that we're ready to resume processing further requests.
            let _ = finished_signal.send(());

            Ok(Response::Commit(response::Commit {
                data: app_hash.into(),
                retain_height: 0u32.into(),
            }))
        }
    }
}

// Wrapper that allows the service to ensure that the current request's response
// future will complete before processing any further requests.
#[derive(Debug)]
struct CompletionTracker {
    // it would be cleaner to use an Option, but we have to box the oneshot
    // future because it won't be Unpin and Service::poll_ready doesn't require
    // a pinned receiver, so tracking the waiting state in a separate bool allows
    // reallocating a new boxed future every time.
    waiting: bool,
    future: tokio_util::sync::ReusableBoxFuture<Result<(), oneshot::error::RecvError>>,
}

impl CompletionTracker {
    /// Returns a oneshot::Sender used to signal completion of the tracked request.
    pub fn start(&mut self) -> oneshot::Sender<()> {
        assert!(!self.waiting);
        let (tx, rx) = oneshot::channel();
        self.waiting = true;
        self.future.set(rx);
        tx
    }

    pub fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        if !self.waiting {
            return Poll::Ready(());
        }

        match self.future.poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(())) => {
                self.waiting = false;
                Poll::Ready(())
            }
            Poll::Ready(Err(_)) => {
                tracing::error!("response future of sequentially-processed request was dropped before completion, likely a bug");
                self.waiting = false;
                Poll::Ready(())
            }
        }
    }
}

impl Default for CompletionTracker {
    fn default() -> Self {
        Self {
            future: tokio_util::sync::ReusableBoxFuture::new(async { Ok(()) }),
            waiting: false,
        }
    }
}

impl Service<Request> for App {
    type Response = Response;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Response, BoxError>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.completion_tracker.poll_ready(cx).map(|_| Ok(()))
    }

    fn call(&mut self, req: Request) -> Self::Future {
        // Create a span for the request, then ensure that the (synchronous)
        // part of the processing is done in that span using `in_scope`.  For
        // requests that are processed asynchronously, we *also* need to use
        // `.instrument(Span::current())` to propagate the span to the future,
        // so that it will be entered every time the future is polled.
        let span = req.create_span();
        span.in_scope(|| {
            let rsp = match req {
                // handled messages
                Request::Info(_) => return self.info().instrument(Span::current()).boxed(),
                Request::Query(query) => Response::Query(self.query(query.data)),
                Request::CheckTx(check_tx) => {
                    // Mark that we want to process CheckTx messages sequentially.
                    // TODO: this requirement is only because we need to avoid
                    // having multiple transactions in the mempool with the same
                    // nullifiers, until we can use ABCI++ and control block
                    // proposals, at which point check_tx can run concurrently.
                    let finished_signal = self.completion_tracker.start();
                    let rsp = self.check_tx(check_tx);
                    return async move {
                        let rsp = rsp.await;
                        tracing::info!(?rsp);
                        let _ = finished_signal.send(());
                        match rsp {
                            Ok(()) => Ok(Response::CheckTx(response::CheckTx::default())),
                            Err(e) => Ok(Response::CheckTx(response::CheckTx {
                                code: 1,
                                log: e.to_string(),
                                ..Default::default()
                            })),
                        }
                    }
                    .instrument(Span::current())
                    .boxed();
                }
                Request::BeginBlock(begin) => Response::BeginBlock(self.begin_block(begin)),
                Request::DeliverTx(deliver_tx) => {
                    // Mark that we want to process DeliverTx messages sequentially.
                    let finished_signal = self.completion_tracker.start();
                    let rsp = self.deliver_tx(deliver_tx.tx);
                    return async move {
                        let rsp = rsp.await;
                        tracing::info!(?rsp);
                        let _ = finished_signal.send(());
                        match rsp {
                            Ok(()) => Ok(Response::DeliverTx(response::DeliverTx::default())),
                            Err(e) => Ok(Response::DeliverTx(response::DeliverTx {
                                code: 1,
                                log: e.to_string(),
                                ..Default::default()
                            })),
                        }
                    }
                    .instrument(Span::current())
                    .boxed();
                }
                Request::EndBlock(end) => Response::EndBlock(self.end_block(end)),
                Request::Commit => return self.commit().instrument(Span::current()).boxed(),

                // Called only once for network genesis, i.e. when the application block height is 0.
                Request::InitChain(init_chain) => {
                    return self
                        .init_genesis(init_chain)
                        .instrument(Span::current())
                        .boxed()
                }

                // unhandled messages
                Request::Flush => Response::Flush,
                Request::Echo(_) => Response::Echo(Default::default()),
                Request::ListSnapshots => Response::ListSnapshots(Default::default()),
                Request::OfferSnapshot(_) => Response::OfferSnapshot(Default::default()),
                Request::LoadSnapshotChunk(_) => Response::LoadSnapshotChunk(Default::default()),
                Request::ApplySnapshotChunk(_) => Response::ApplySnapshotChunk(Default::default()),
            };
            tracing::info!(?rsp);
            async move { Ok(rsp) }.boxed()
        })
    }
}

/*
// TODO: restore this test after writing a state facade (?)
//   OR: don't write a state facade, and test the actual code using test pg states
#[cfg(test)]
mod tests {
    use super::*;

    use ark_ff::Zero;
    use rand_core::OsRng;

    use penumbra_crypto::{keys::SpendKey, memo::MemoPlaintext, Fq, Note, Value};

    #[test]
    fn test_transaction_verification_fails_for_dummy_merkle_tree() {

        let mut app = App::default();

        let mut rng = OsRng;
        let sk_sender = SpendKey::generate(&mut rng);
        let fvk_sender = sk_sender.full_viewing_key();
        let ovk_sender = fvk_sender.outgoing();

        let sk_recipient = SpendKey::generate(&mut rng);
        let fvk_recipient = sk_recipient.full_viewing_key();
        let ivk_recipient = fvk_recipient.incoming();
        let (dest, _dtk_d) = ivk_recipient.payment_address(0u64.into());

        let merkle_root = merkle::Root(Fq::zero());
        let mut merkle_siblings = Vec::new();
        for _i in 0..merkle::DEPTH {
            merkle_siblings.push(note::Commitment(Fq::zero()))
        }
        let dummy_merkle_path: merkle::Path = (merkle::DEPTH, merkle_siblings);

        let value_to_send = Value {
            amount: 10,
            asset_id: b"penumbra".as_ref().into(),
        };
        let dummy_note = Note::new(
            *dest.diversifier(),
            *dest.transmission_key(),
            value_to_send,
            Fq::zero(),
        )
        .expect("transmission key is valid");

        let transaction = Transaction::build_with_root(merkle_root)
            .set_fee(20)
            .set_chain_id("penumbra".to_string())
            .add_output(
                &mut rng,
                &dest,
                value_to_send,
                MemoPlaintext::default(),
                ovk_sender,
            )
            .add_spend(&mut rng, sk_sender, dummy_merkle_path, dummy_note, 0.into())
            .finalize(&mut rng);

        // The merkle path is invalid, so this transaction should not verify.
        assert!(!app.verify_transaction(transaction.unwrap()));
    }
}
    */
