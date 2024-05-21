use ckb_jsonrpc_types::TransactionView;
use ckb_types::H256;
use jsonrpsee::http_client::HttpClient;
use molecule::{
    bytes::Buf,
    prelude::{Entity, Reader as _},
};
use rayon::prelude::{IntoParallelRefIterator as _, ParallelIterator as _};
use sea_orm::{
    prelude::{ActiveModelTrait as _, DbConn, EntityTrait as _},
    Set,
};
use tokio::sync::mpsc;
use tokio_stream::{wrappers::ReceiverStream, StreamExt as _};
use tracing::debug;

use crate::{
    fetcher::Fetcher,
    schemas::{blockchain, rgbpp},
};

pub struct RgbppIndexer {
    db: DbConn,
    stream: ReceiverStream<TransactionView>,
    fetcher: Fetcher<HttpClient>,
}

impl RgbppIndexer {
    pub fn new(
        db: &DbConn,
        fetcher: &Fetcher<HttpClient>,
    ) -> (Self, mpsc::Sender<TransactionView>) {
        let (tx, rx) = mpsc::channel(100);
        (
            Self {
                db: db.clone(),
                fetcher: fetcher.clone(),
                stream: ReceiverStream::new(rx),
            },
            tx,
        )
    }

    pub async fn index(self) -> Result<(), anyhow::Error> {
        let Self {
            db,
            mut stream,
            fetcher,
        } = self;

        while let Some(tx) = stream.next().await {
            index_rgbpp_lock(&fetcher, &db, tx).await?;
        }

        Ok(())
    }
}

async fn index_rgbpp_lock(
    fetcher: &Fetcher<HttpClient>,
    db: &DbConn,
    tx: TransactionView,
) -> anyhow::Result<()> {
    debug!("tx: {}", hex::encode(tx.hash.as_bytes()));

    let rgbpp_unlocks = tx
        .inner
        .witnesses
        .par_iter()
        .filter_map(|witness| blockchain::WitnessArgsReader::from_slice(witness.as_bytes()).ok())
        .filter_map(|witness_args| {
            witness_args
                .to_entity()
                .lock()
                .to_opt()
                .and_then(|lock_witness| {
                    rgbpp::RGBPPUnlockReader::from_slice(lock_witness.raw_data().as_ref())
                        .ok()
                        .map(|unlock| unlock.to_entity())
                })
        })
        .collect::<Vec<_>>();

    for unlock in rgbpp_unlocks {
        debug!("unlock: {}", unlock);

        upsert_rgbpp_unlock(db, &unlock, tx.hash.clone()).await?;
    }

    let pre_outputs = fetcher.get_outputs(tx.inner.inputs).await?;

    let inputs = pre_outputs
        .par_iter()
        .filter_map(|output| {
            rgbpp::RGBPPLockReader::from_slice(output.lock.args.as_bytes())
                .ok()
                .map(|reader| reader.to_entity())
        })
        .collect::<Vec<_>>();

    let outputs = tx
        .inner
        .outputs
        .par_iter()
        .filter_map(|output| {
            rgbpp::RGBPPLockReader::from_slice(output.lock.args.as_bytes())
                .ok()
                .map(|reader| reader.to_entity())
        })
        .collect::<Vec<_>>();

    let locks = [inputs, outputs].concat();

    for lock in locks {
        debug!("lock: {}", lock);

        upsert_rgbpp_lock(db, &lock, tx.hash.clone()).await?;
    }

    Ok(())
}

impl rgbpp::RGBPPLock {
    fn lock_id(&self) -> Vec<u8> {
        blockchain::Bytes::new_unchecked(self.as_bytes())
            .raw_data()
            .to_vec()
    }
}

async fn upsert_rgbpp_lock(
    db: &DbConn,
    rgbpp_lock: &rgbpp::RGBPPLock,
    tx: H256,
) -> anyhow::Result<()> {
    use crate::entity::rgbpp_locks;

    let lock_id = rgbpp_lock.lock_id();
    let lock_exists = rgbpp_locks::Entity::find_by_id(lock_id.clone())
        .one(db)
        .await?
        .is_some();

    if !lock_exists {
        let mut txid = rgbpp_lock.btc_txid().as_bytes().to_vec();
        txid.reverse();
        // Insert rgbpp lock
        rgbpp_locks::ActiveModel {
            lock_id: Set(lock_id),
            out_index: Set(rgbpp_lock.out_index().raw_data().get_u32_le() as i32),
            btc_txid: Set(txid),
            tx: Set(tx.0.to_vec()),
        }
        .insert(db)
        .await?;
    }

    Ok(())
}

impl rgbpp::RGBPPUnlock {
    fn unlock_id(&self) -> Vec<u8> {
        let hash = ckb_hash::blake2b_256(self.as_bytes());
        hash.to_vec()
    }
}

async fn upsert_rgbpp_unlock(
    db: &DbConn,
    rgbpp_unlock: &rgbpp::RGBPPUnlock,
    tx: H256,
) -> anyhow::Result<()> {
    use crate::entity::rgbpp_unlocks;

    let unlock_id = rgbpp_unlock.unlock_id();
    let unlock_exists = rgbpp_unlocks::Entity::find_by_id(unlock_id.clone())
        .one(db)
        .await?
        .is_some();

    if !unlock_exists {
        // Insert rgbpp lock
        rgbpp_unlocks::ActiveModel {
            unlock_id: Set(unlock_id),
            version: Set(rgbpp_unlock.version().raw_data().get_u16_le() as i16),
            input_len: Set(rgbpp_unlock.extra_data().input_len().as_bytes().get_u8() as i16),
            output_len: Set(rgbpp_unlock.extra_data().output_len().as_bytes().get_u8() as i16),
            btc_tx: Set(rgbpp_unlock.btc_tx().as_bytes().to_vec()),
            btc_tx_proof: Set(rgbpp_unlock.btc_tx_proof().as_bytes().to_vec()),
            tx: Set(tx.0.to_vec()),
        }
        .insert(db)
        .await?;
    }

    Ok(())
}
