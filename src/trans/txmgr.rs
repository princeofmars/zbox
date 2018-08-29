use std::collections::HashMap;
use std::fmt::{self, Debug};
use std::sync::{Arc, RwLock};

use linked_hash_map::LinkedHashMap;

use super::trans::{Action, Trans, TransRef, TransableRef};
use super::wal::{EntityType, WalQueue};
use super::{Eid, Txid};
use base::IntoRef;
use error::{Error, Result};
use volume::{AllocatorRef, Arm, Armor, VolumeArmor, VolumeRef};

/// Tranaction manager
#[derive(Default)]
pub struct TxMgr {
    // txid watermark
    txid_wmark: Txid,

    // transaction list
    txs: LinkedHashMap<Txid, TransRef>,

    // entity tx map
    ents: HashMap<Eid, Txid>,

    // wal queue and wal queue armor
    walq: WalQueue,
    walq_armor: VolumeArmor<WalQueue>,

    allocator: AllocatorRef,
    vol: VolumeRef,
}

impl TxMgr {
    pub fn new(walq_id: &Eid, vol: &VolumeRef) -> Self {
        let allocator = {
            let vol = vol.read().unwrap();
            vol.allocator()
        };
        TxMgr {
            txid_wmark: Txid::from(0),
            txs: LinkedHashMap::new(),
            ents: HashMap::new(),
            walq: WalQueue::new(walq_id, vol),
            walq_armor: VolumeArmor::new(vol),
            allocator,
            vol: vol.clone(),
        }
    }

    // save wal queue to volume
    fn save_walq(&mut self) -> Result<()> {
        // reserve one block ahead for the wal queue, because wal queue itself
        // will consume a block
        let blk_wmark = {
            let mut allocator = self.allocator.write().unwrap();
            allocator.reserve(1)
        };

        // save watermarks to wal queue and save it
        self.walq.set_watermarks(self.txid_wmark.val(), blk_wmark);
        self.walq_armor.save_item(&mut self.walq)?;

        // make sure the block watermark is correct
        {
            let allocator = self.allocator.read().unwrap();
            assert_eq!(allocator.block_wmark(), blk_wmark);
        }

        Ok(())
    }

    /// Open transaction manager
    pub fn open(walq_id: &Eid, vol: &VolumeRef) -> Result<Self> {
        let mut txmgr = TxMgr::new(walq_id, vol);

        // load and open wal queue
        txmgr.walq = txmgr.walq_armor.load_item(walq_id)?;
        txmgr.walq.open(vol);

        // restore water marks
        let (txid_wmark, blk_wmark) = txmgr.walq.watermarks();
        txmgr.txid_wmark = Txid::from(txid_wmark);
        {
            let mut allocator = txmgr.allocator.write().unwrap();
            allocator.set_block_wmark(blk_wmark);
        }

        // now redo abort tx if any
        if txmgr.walq.cold_redo_abort(vol)? > 0 {
            // if there are any txs are successfully redoed abort,
            // save the wal queue
            txmgr.save_walq()?;
        }

        debug!(
            "txmgr opened, watermarks: txid: {}, block: {}",
            txid_wmark, blk_wmark
        );

        Ok(txmgr)
    }

    /// Begin a transaction
    pub fn begin_trans(txmgr: &TxMgrRef) -> Result<TxHandle> {
        // check if current thread is already in transaction
        if Txid::is_in_trans() {
            return Err(Error::InTrans);
        }

        let mut tm = txmgr.write().unwrap();
        let vol = tm.vol.clone();

        // try to redo abort tx if any tx failed abortion before,
        if tm.walq.hot_redo_abort(&vol)? > 0 {
            // if there are any txs are successfully redone abort,
            // save the wal queue
            tm.save_walq()?;
        }

        // get next txid, here we marked this thread as in tx
        let txid = tm.txid_wmark.next();
        debug!("begin tx#{}", txid);

        // begin a transaction in wal queue
        tm.walq.begin_trans(txid);
        if let Err(err) = tm.save_walq() {
            // if save wal queue failed, clean up it and remove thread tx mark
            tm.walq.end_abort(txid);
            Txid::reset_current();
            debug!("tx#{} aborted before start", txid);
            return Err(err);
        }

        // create a new transaction and add it to transaction manager
        let tx = Trans::new(txid, &vol).into_ref();
        tm.txs.insert(txid, tx.clone());

        // start the transaction
        let result = {
            let mut tx = tx.write().unwrap();
            tx.begin_trans()
        };
        if let Err(err) = result {
            tm.abort_trans(txid);
            return Err(err);
        }

        Ok(TxHandle {
            txid,
            txmgr: txmgr.clone(),
        })
    }

    /// Add entity to transaction
    pub fn add_to_trans(
        &mut self,
        id: &Eid,
        txid: Txid,
        entity: TransableRef,
        action: Action,
        ent_type: EntityType,
        arm: Arm,
    ) -> Result<()> {
        let cur_txid = self.ents.entry(id.clone()).or_insert(txid);
        if *cur_txid != txid {
            // entity is already in other transaction
            return Err(Error::InTrans);
        }

        // get tx and add entity to tx
        let txref = self.txs.get(&txid).ok_or(Error::NoTrans)?;
        let mut tx = txref.write().unwrap();
        tx.add_entity(id, entity, action, ent_type, arm)
    }

    #[inline]
    fn remove_trans(&mut self, txid: Txid) {
        self.txs.remove(&txid);
        self.ents.retain(|_, &mut v| v != txid);
    }

    // commit transaction
    fn commit_trans(&mut self, txid: Txid) -> Result<()> {
        let result = {
            let tx_ref = self.txs.get(&txid).unwrap().clone();
            let mut tx = tx_ref.write().unwrap();

            // commit tx, if any errors then abort the tx
            match tx
                .commit(&self.vol)
                .and_then(|wal| self.walq.end_trans(wal))
                .and(self.save_walq())
            {
                Ok(_) => {
                    tx.complete_commit();
                    debug!("tx#{} committed", txid);
                    Ok(())
                }
                Err(err) => Err(err),
            }
        };

        if result.is_err() {
            // error happened during commit, abort the tx
            self.abort_trans(txid);
        } else {
            // remove tx from tx manager and remove txid from current thread
            self.remove_trans(txid);
            Txid::reset_current();
        }

        // return the original result during commit
        result
    }

    // abort transaction
    fn abort_trans(&mut self, txid: Txid) {
        debug!("abort tx#{}", txid);

        {
            let tx_ref = self.txs.get(&txid).unwrap().clone();
            let mut tx = tx_ref.write().unwrap();
            let wal = tx.get_wal();

            self.walq.begin_abort(&wal);
            match tx
                .abort(&self.vol)
                .and(Ok(self.walq.end_abort(txid)))
                .and(self.save_walq())
            {
                Ok(_) => debug!("tx#{} aborted", txid),
                Err(err) => warn!("abort tx#{} failed: {}", txid, err),
            }
        }

        // remove tx from tx manager and remove txid from current thread
        self.remove_trans(txid);
        Txid::reset_current();
    }
}

impl Debug for TxMgr {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("TxMgr")
            .field("txid_wmark", &self.txid_wmark)
            .field("txs", &self.txs)
            .field("ents", &self.ents)
            .field("walq", &self.walq)
            .finish()
    }
}

impl IntoRef for TxMgr {}

/// TxMgr reference type
pub type TxMgrRef = Arc<RwLock<TxMgr>>;

// Transaction handle
#[derive(Debug, Default, Clone)]
pub struct TxHandle {
    pub txid: Txid,
    pub txmgr: TxMgrRef,
}

impl TxHandle {
    /// Run operations in transaction and continue
    pub fn run<F>(&self, oper: F) -> Result<()>
    where
        F: FnOnce() -> Result<()>,
    {
        match oper() {
            Ok(_) => Ok(()),
            Err(err) => self.abort(err),
        }
    }

    /// Run operations in transaction and commit
    pub fn run_all<F>(&self, oper: F) -> Result<()>
    where
        F: FnOnce() -> Result<()>,
    {
        match oper() {
            Ok(_) => self.commit(),
            Err(err) => self.abort(err),
        }
    }

    /// Commit a transaction
    pub fn commit(&self) -> Result<()> {
        let mut tm = self.txmgr.write().unwrap();
        tm.commit_trans(self.txid)
    }

    /// Abort a transaction
    fn abort(&self, err: Error) -> Result<()> {
        let mut tm = self.txmgr.write().unwrap();
        tm.abort_trans(self.txid);

        // return the original error
        Err(err)
    }
}

#[cfg(test)]
mod tests {
    extern crate tempdir;

    use self::tempdir::TempDir;
    use super::*;

    use base::init_env;
    use fs::Config;
    use trans::cow::{CowRef, Cowable, IntoCow};
    use trans::TxMgr;
    use volume::{ArmAccess, Volume};

    #[allow(dead_code)]
    fn setup_mem_vol() -> VolumeRef {
        init_env();
        let uri = "mem://foo".to_string();
        let mut vol = Volume::new(&uri).unwrap();
        vol.init("pwd", &Config::default(), &Vec::new()).unwrap();
        vol.into_ref()
    }

    fn setup_file_vol() -> (VolumeRef, TempDir) {
        init_env();
        let tmpdir = TempDir::new("zbox_test").expect("Create temp dir failed");
        let uri = format!("file://{}", tmpdir.path().display());
        let mut vol = Volume::new(&uri).unwrap();
        vol.init("pwd", &Config::default(), &Vec::new()).unwrap();
        (vol.into_ref(), tmpdir)
    }

    #[derive(Debug, Default, Clone, Deserialize, Serialize)]
    struct Obj {
        val: u8,
    }

    impl Obj {
        fn new(val: u8) -> Self {
            Obj { val }
        }

        fn ensure(cow: &CowRef<Obj>, val: u8, arm: Arm) {
            let a = cow.read().unwrap();
            assert_eq!(a.val, val);
            assert_eq!(a.arm(), arm);
        }
    }

    impl Cowable for Obj {}
    impl<'d> IntoCow<'d> for Obj {}

    #[test]
    fn trans_oper() {
        //let vol = setup_mem_vol();
        let (vol, _tmpdir) = setup_file_vol();
        let tm = TxMgr::new(&Eid::new(), &vol).into_ref();
        let val = 42;
        let val2 = 43;
        let mut a = Arc::default();
        let mut b = Arc::default();

        // tx #1, new
        let tx = TxMgr::begin_trans(&tm).unwrap();
        tx.run_all(|| {
            a = Obj::new(val).into_cow(&tm)?;
            Obj::ensure(&a, val, Arm::Right);
            Ok(())
        }).unwrap();
        Obj::ensure(&a, val, Arm::Right);

        // tx #2, new and update
        let tx = TxMgr::begin_trans(&tm).unwrap();
        tx.run_all(|| {
            let mut a_cow = a.write().unwrap();
            let a = a_cow.make_mut()?;
            a.val = val2;
            b = Obj::new(val).into_cow(&tm)?;
            Ok(())
        }).unwrap();
        Obj::ensure(&a, val2, Arm::Left);
        Obj::ensure(&b, val, Arm::Right);

        // tx #3, update and delete
        let tx = TxMgr::begin_trans(&tm).unwrap();
        tx.run_all(|| {
            {
                let mut a_cow = a.write().unwrap();
                a_cow.make_del()?;
            }
            drop(a);
            let mut b_cow = b.write().unwrap();
            let b = b_cow.make_mut()?;
            b.val = val2;
            Ok(())
        }).unwrap();
        Obj::ensure(&b, val2, Arm::Left);

        // tx #4, recycle tx#2
        let tx = TxMgr::begin_trans(&tm).unwrap();
        tx.run_all(|| {
            let mut b_cow = b.write().unwrap();
            let b = b_cow.make_mut()?;
            b.val = val;
            Ok(())
        }).unwrap();
        Obj::ensure(&b, val, Arm::Right);

        // tx #5, recyle tx#3
        let tx = TxMgr::begin_trans(&tm).unwrap();
        tx.run_all(|| {
            let mut b_cow = b.write().unwrap();
            let b = b_cow.make_mut()?;
            b.val = val2;
            Ok(())
        }).unwrap();
        Obj::ensure(&b, val2, Arm::Left);

        // more txs
        for i in 0..5 {
            let tx = TxMgr::begin_trans(&tm).unwrap();
            tx.run_all(|| {
                let mut b_cow = b.write().unwrap();
                let b = b_cow.make_mut()?;
                b.val = val2 + i;
                Ok(())
            }).unwrap();
            let arm = if i % 2 == 0 { Arm::Right } else { Arm::Left };
            Obj::ensure(&b, val2 + i, arm);
        }
    }

    #[test]
    fn trans_abort() {
        //let vol = setup_mem_vol();
        let (vol, _tmpdir) = setup_file_vol();
        let tm = TxMgr::new(&Eid::new(), &vol).into_ref();
        let val = 42;
        let mut a = Arc::default();
        let mut b = Arc::default();

        // tx #1, abort in the middle of tx
        let tx = TxMgr::begin_trans(&tm).unwrap();
        assert_eq!(
            tx.run(|| {
                a = Obj::new(val).into_cow(&tm)?;
                Err(Error::NotFound)
            }).unwrap_err(),
            Error::NotFound
        );
        {
            let tm = tm.read().unwrap();
            assert!(tm.txs.is_empty());
        }

        // tx #2, abort during committing
        let tx = TxMgr::begin_trans(&tm).unwrap();
        assert_eq!(
            tx.run_all(|| {
                b = Obj::new(val).into_cow(&tm)?;
                let mut a_cow = a.write().unwrap();
                a_cow.make_del()?;
                Ok(())
            }).unwrap_err(),
            Error::InUse
        );
        {
            let tm = tm.read().unwrap();
            assert!(tm.txs.is_empty());
        }
    }
}
