use anyhow::{Context as _, Result};
use penumbra_component::Context;
use penumbra_crypto::note;
use penumbra_proto::{
    client::{
        oblivious::oblivious_query_client::ObliviousQueryClient,
        specific::specific_query_client::SpecificQueryClient,
    },
    Protobuf,
};
use penumbra_transaction::{plan::TransactionPlan, Transaction};
use penumbra_view::ViewClient;
use rand::Rng;
use rand_core::OsRng;
use std::future::Future;
use tonic::transport::Channel;
use tracing::instrument;

use crate::App;

impl App {
    pub async fn build_and_submit_transaction(
        &mut self,
        plan: TransactionPlan,
    ) -> anyhow::Result<()> {
        let self_addressed_output = plan
            .output_plans()
            .find(|output| output.is_viewed_by(self.fvk.incoming()))
            .map(|output| output.output_note().commit());

        let tx = self.build_transaction(plan).await?;

        self.submit_transaction(&tx, self_addressed_output).await
    }

    pub fn build_transaction<'a>(
        &'a mut self,
        plan: TransactionPlan,
    ) -> impl Future<Output = Result<Transaction>> + 'a {
        penumbra_wallet::build_transaction(
            &self.fvk,
            &mut self.view,
            &mut self.custody,
            OsRng,
            plan,
        )
    }

    /// Submits a transaction to the network.
    ///
    /// # Returns
    ///
    /// - if `await_detection_of` is `Some`, returns `Ok` after the specified note has been detected by the view service, implying transaction finality.
    /// - if `await_detection_of` is `None`, returns `Ok` after the transaction has been accepted by the node it was sent to.
    #[instrument(skip(self, transaction, await_detection_of))]
    pub async fn submit_transaction(
        &mut self,
        transaction: &Transaction,
        await_detection_of: Option<note::Commitment>,
    ) -> Result<(), anyhow::Error> {
        println!("pre-checking transaction...");
        use penumbra_component::Component;
        let ctx = Context::new();
        pd::App::check_tx_stateless(ctx.clone(), transaction)
            .context("transaction pre-submission checks failed")?;

        println!("broadcasting transaction...");

        let client = reqwest::Client::new();
        let req_id: u8 = rand::thread_rng().gen();
        let rsp: serde_json::Value = client
            .post(self.tendermint_url.clone())
            .json(&serde_json::json!(
                {
                    "method": "broadcast_tx_sync",
                    "params": [&transaction.encode_to_vec()],
                    "id": req_id,
                }
            ))
            .send()
            .await?
            .json()
            .await?;

        tracing::info!("{}", rsp);

        // Sometimes the result is in a result key, and sometimes it's bare? (??)
        let result = rsp.get("result").unwrap_or(&rsp);

        let code = result
            .get("code")
            .and_then(|c| c.as_i64())
            .ok_or_else(|| anyhow::anyhow!("could not parse JSON response"))?;

        if code != 0 {
            let log = result
                .get("log")
                .and_then(|l| l.as_str())
                .ok_or_else(|| anyhow::anyhow!("could not parse JSON response"))?;

            return Err(anyhow::anyhow!(
                "Error submitting transaction: code {}, log: {}",
                code,
                log
            ));
        }

        if let Some(note_commitment) = await_detection_of {
            // putting two spaces in makes the ellipsis line up with the above
            println!("confirming transaction  ...");
            let fvk_hash = self.fvk.hash();
            tokio::time::timeout(
                std::time::Duration::from_secs(20),
                self.view()
                    .await_note_by_commitment(fvk_hash, note_commitment),
            )
            .await
            .context("timeout waiting to detect outputs of submitted transaction")?
            .context("error while waiting for detection of submitted transaction")?;
            println!("transaction confirmed and detected");
        } else {
            println!("transaction submitted successfully");
        }

        Ok(())
    }

    /// Submits a transaction to the network, returning `Ok` as soon as the
    /// transaction has been submitted, rather than waiting to learn whether the
    /// node accepted it.
    #[instrument(skip(self, transaction))]
    pub async fn submit_transaction_unconfirmed(
        &self,
        transaction: &Transaction,
    ) -> Result<(), anyhow::Error> {
        println!("broadcasting transaction...");

        let client = reqwest::Client::new();
        let req_id: u8 = rand::thread_rng().gen();
        let rsp: serde_json::Value = client
            .post(self.tendermint_url.clone())
            .json(&serde_json::json!(
                {
                    "method": "broadcast_tx_async",
                    "params": [&transaction.encode_to_vec()],
                    "id": req_id,
                }
            ))
            .send()
            .await?
            .json()
            .await?;

        tracing::info!("{}", rsp);

        Ok(())
    }

    pub async fn specific_client(&self) -> Result<SpecificQueryClient<Channel>, anyhow::Error> {
        SpecificQueryClient::connect(self.pd_url.as_ref().to_owned())
            .await
            .map_err(Into::into)
    }

    pub async fn oblivious_client(&self) -> Result<ObliviousQueryClient<Channel>, anyhow::Error> {
        ObliviousQueryClient::connect(self.pd_url.as_ref().to_owned())
            .await
            .map_err(Into::into)
    }
}
