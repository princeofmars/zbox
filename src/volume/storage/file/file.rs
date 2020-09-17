use std::error::Error as StdError;
use std::fmt::{self, Display};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::io::{Error as IoError, ErrorKind, Read, Result as IoResult, Write};
use std::cmp::min;
use std::env;

use error::{Error, Result};
use base::Time;
use base::crypto::{Crypto, Key};
use base::utils::align_ceil;
use trans::{Eid, Txid};
use volume::storage::Storage;
use super::{load_obj, remove_file, save_obj};
use super::span::Span;
use super::sector::{LocId, SectorMgr, Space, BLK_SIZE};
use super::emap::Emap;
use super::vio::imp as vio_imp;

// maximum snapshot count
const MAX_SNAPSHOT_CNT: usize = 2;

// snapshot
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
struct Snapshot {
    seq: u64,
    txid: Txid,
    wmark: u64,
    recycle: Vec<Space>,
    tm: Time,

    #[serde(skip_serializing, skip_deserializing, default)] base: PathBuf,

    #[serde(skip_serializing, skip_deserializing, default)] skey: Key,

    #[serde(skip_serializing, skip_deserializing, default)] crypto: Crypto,
}

impl Snapshot {
    const DIR_NAME: &'static str = "snapshot";

    fn new(
        seq: u64,
        txid: Txid,
        wmark: u64,
        recycle: Vec<Space>,
        base: PathBuf,
        skey: &Key,
        crypto: &Crypto,
    ) -> Self {
        Snapshot {
            seq,
            txid,
            wmark,
            recycle,
            tm: Time::now(),
            base,
            skey: skey.clone(),
            crypto: crypto.clone(),
        }
    }

    fn init(base: &Path) -> Result<()> {
        vio_imp::create_dir(base.join(Snapshot::DIR_NAME))?;
        Ok(())
    }

    fn path(base: &Path, txid: Txid) -> PathBuf {
        base.join(Snapshot::DIR_NAME).join(&txid.to_string())
    }

    fn save(&self) -> Result<()> {
        let file_path = Snapshot::path(&self.base, self.txid);
        save_obj(self, file_path, &self.skey, &self.crypto)
    }

    fn load(
        base: &Path,
        txid: Txid,
        skey: &Key,
        crypto: &Crypto,
    ) -> Result<Snapshot> {
        let file_path = Snapshot::path(base, txid);
        let mut snapshot: Snapshot = load_obj(file_path, skey, crypto)?;
        snapshot.base = base.to_path_buf();
        Ok(snapshot)
    }

    fn cleanup(base: &Path, txid: Txid) -> Result<()> {
        remove_file(Snapshot::path(base, txid))?;
        Ok(())
    }
}

// transaction session status
#[derive(Debug, PartialEq, Clone, Copy)]
enum TxStatus {
    Init,      // initial status
    Started,   // transaction started
    Prepare,   // committing preparation started
    Recycle,   // recycling started
    Committed, // transaction committed
    Dispose,   // dispose a committed transaction
}

impl Default for TxStatus {
    #[inline]
    fn default() -> Self {
        TxStatus::Init
    }
}

impl Display for TxStatus {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            TxStatus::Init => write!(f, "init"),
            TxStatus::Started => write!(f, "started"),
            TxStatus::Prepare => write!(f, "prepare"),
            TxStatus::Recycle => write!(f, "recycle"),
            TxStatus::Committed => write!(f, "committed"),
            TxStatus::Dispose => write!(f, "dispose"),
        }
    }
}

impl<'a> From<&'a str> for TxStatus {
    fn from(val: &str) -> TxStatus {
        match val {
            "started" => TxStatus::Started,
            "prepare" => TxStatus::Prepare,
            "recycle" => TxStatus::Recycle,
            "committed" => TxStatus::Committed,
            "dispose" => TxStatus::Dispose,
            _ => unreachable!(),
        }
    }
}

// transaction session history item
#[derive(Debug, Default)]
struct SessionHistItem {
    seq: u64,
    txid: Txid,
    status: TxStatus,
}

impl SessionHistItem {
    #[inline]
    fn is_completed(&self) -> bool {
        self.status == TxStatus::Committed || self.status == TxStatus::Dispose
    }
}

// transaction session
#[derive(Debug)]
struct Session {
    seq: u64,
    txid: Txid,
    status: TxStatus,
    wmark: u64,
    emap: Emap,
    deleted: HashSet<Eid>, // deleted entities
    recycle: Vec<Space>,
    base: PathBuf,
    skey: Key,
    crypto: Crypto,
}

impl Session {
    const DIR_NAME: &'static str = "session";

    fn new(
        seq: u64,
        txid: Txid,
        base: &Path,
        skey: &Key,
        crypto: &Crypto,
    ) -> Self {
        let mut ret = Session {
            seq,
            txid,
            status: TxStatus::Init,
            wmark: 0,
            emap: Emap::new(base, txid),
            deleted: HashSet::new(),
            recycle: Vec::new(),
            base: base.to_path_buf(),
            skey: skey.clone(),
            crypto: crypto.clone(),
        };
        ret.emap.set_crypto_key(crypto, skey);
        ret
    }

    fn init(base: &Path) -> Result<()> {
        vio_imp::create_dir(base.join(Session::DIR_NAME))?;
        Ok(())
    }

    fn path(base: &Path, txid: Txid, seq: u64) -> PathBuf {
        let stem = format!("{}-{}", txid, seq);
        base.join(Session::DIR_NAME).join(stem)
    }

    #[inline]
    fn is_committing(&self) -> bool {
        self.status == TxStatus::Prepare || self.status == TxStatus::Recycle
    }

    fn status_path(&self, status: TxStatus) -> PathBuf {
        Session::path(&self.base, self.txid, self.seq)
            .with_extension(status.to_string())
    }

    fn switch_to_status(&mut self, to_status: TxStatus) -> Result<()> {
        vio_imp::rename(
            self.status_path(self.status),
            self.status_path(to_status),
        )?;
        self.status = to_status;
        Ok(())
    }

    fn status_started(&mut self) -> Result<()> {
        let to_status = TxStatus::Started;
        let file_path = self.status_path(to_status);
        vio_imp::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(file_path)?;
        self.status = to_status;
        Ok(())
    }

    #[inline]
    fn status_prepare(&mut self) -> Result<()> {
        self.switch_to_status(TxStatus::Prepare)
    }

    #[inline]
    fn status_recycle(&mut self) -> Result<()> {
        self.switch_to_status(TxStatus::Recycle)
    }

    #[inline]
    fn status_committed(&mut self) -> Result<()> {
        self.switch_to_status(TxStatus::Committed)
    }

    // allocate space
    fn alloc(&mut self, size: usize) -> Space {
        let blk_cnt = align_ceil(size, BLK_SIZE) / BLK_SIZE;
        let begin = self.wmark;
        self.wmark += blk_cnt as u64;
        let spans = Span::new(begin, self.wmark, 0).into_span_list(size);
        Space::new(self.txid, spans)
    }

    #[inline]
    fn take_snapshot(&self) -> Snapshot {
        Snapshot::new(
            self.seq,
            self.txid,
            self.wmark,
            self.recycle.clone(),
            self.base.clone(),
            &self.skey,
            &self.crypto,
        )
    }

    // load session history
    fn load_history(base: &Path) -> Result<Vec<SessionHistItem>> {
        let mut hist = Vec::new();

        for entry in vio_imp::read_dir(base.join(Session::DIR_NAME))? {
            let path = entry?.path();
            let comps = path.file_stem()
                .unwrap()
                .to_str()
                .unwrap()
                .split("-")
                .collect::<Vec<&str>>();
            let mut item = SessionHistItem::default();
            item.txid = Txid::from(comps[0].parse::<u64>().unwrap());
            item.seq = comps[1].parse::<u64>().unwrap();
            item.status =
                TxStatus::from(path.extension().unwrap().to_str().unwrap());
            hist.push(item);
        }

        // sort history by seq
        hist.sort_by_key(|h| h.seq);

        Ok(hist)
    }

    // mark a committed session as dispose status
    fn dispose(base: &Path, txid: Txid) -> Result<()> {
        let prefix = format!("{}-", txid);
        for entry in vio_imp::read_dir(base.join(Session::DIR_NAME))? {
            let entry = entry?;
            if entry.file_name().to_str().unwrap().starts_with(&prefix) {
                let mut new_path = entry.path();
                new_path.set_extension(TxStatus::Dispose.to_string());
                vio_imp::rename(&entry.path(), &new_path)?;
                break;
            }
        }
        Ok(())
    }

    fn cleanup(base: &Path, txid: Txid) -> Result<()> {
        let prefix = format!("{}-", txid);
        for entry in vio_imp::read_dir(base.join(Session::DIR_NAME))? {
            let entry = entry?;
            if entry.file_name().to_str().unwrap().starts_with(&prefix) {
                remove_file(entry.path())?;
                break;
            }
        }
        Ok(())
    }
}

/// File Storage
#[derive(Debug)]
pub struct FileStorage {
    // sequence number
    seq: u64,

    // path config
    base: PathBuf,
    super_blk_path: PathBuf,
    lock_path: PathBuf,

    // base entity map
    emap: Emap,

    // transaction sessions
    sessions: HashMap<Txid, Session>,

    // snapshot list
    snapshots: VecDeque<Snapshot>,

    // sector manager
    secmgr: SectorMgr,

    skey: Key, // storage encryption key
    crypto: Crypto,
}

impl FileStorage {
    pub fn new(base: &Path) -> Self {
        FileStorage {
            seq: 0,
            base: base.to_path_buf(),
            super_blk_path: base.join("super"),
            lock_path: PathBuf::new(),
            emap: Emap::new(base, Txid::new_empty()),
            sessions: HashMap::new(),
            snapshots: VecDeque::new(),
            secmgr: SectorMgr::new(base),
            skey: Key::new_empty(),
            crypto: Crypto::default(),
        }
    }

    // set key for storage and it's components
    fn set_crypto_key(&mut self, crypto: &Crypto, skey: &Key) -> Result<()> {
        self.crypto = crypto.clone();
        self.skey = skey.clone();
        self.emap.set_crypto_key(crypto, skey);
        self.secmgr.set_crypto_key(crypto, skey)
    }

    // set lock file path
    fn lock_storage(&mut self, volume_id: &Eid) -> Result<()> {
        let lock_file = format!("zbox_{}.lock", volume_id.to_short_string());
        let mut lock_path = env::temp_dir();
        lock_path.push(lock_file);

        if lock_path.exists() {
            return Err(Error::Opened);
        }

        // create lock file
        vio_imp::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)?;
        self.lock_path = lock_path;
        Ok(())
    }

    // recycle retired snapshot
    fn recycle(&mut self) -> Result<()> {
        while self.snapshots.len() > MAX_SNAPSHOT_CNT {
            {
                // the oldest snapshot is the one need to be retired
                let retired = self.snapshots.front().unwrap();
                debug!(
                    "start recycling snapshot#{}, entities_cnt: {}",
                    retired.txid,
                    retired.recycle.len()
                );
                Session::dispose(&self.base, retired.txid)?;
                self.secmgr.recycle(&retired.recycle)?;
                Emap::cleanup(&self.base, retired.txid)?;
                Snapshot::cleanup(&self.base, retired.txid)?;
                Session::cleanup(&self.base, retired.txid)?;
            }
            self.snapshots.pop_front();
        }
        Ok(())
    }

    // cleanup session
    fn cleanup(&mut self, txid: Txid, status: TxStatus) -> Result<()> {
        debug!("cleanup tx#{}, status: {}", txid, status);

        fn do_cleanup(this: &mut FileStorage, txid: Txid) -> Result<()> {
            this.secmgr.cleanup(txid)?;
            Emap::cleanup(&this.base, txid)?;
            Snapshot::cleanup(&this.base, txid)?;
            Ok(())
        }

        match status {
            TxStatus::Started => self.secmgr.cleanup(txid)?,
            TxStatus::Prepare => do_cleanup(self, txid)?,
            TxStatus::Recycle => {
                // do cleanup and redo recyle
                do_cleanup(self, txid)?;
                self.recycle()?;
            }
            _ => unreachable!(),
        }

        // remove snapshot
        if self.snapshots.back().map_or(false, |s| s.txid == txid) {
            self.snapshots.pop_back();
        }

        // remove session
        Session::cleanup(&self.base, txid)?;
        self.sessions.remove(&txid);

        Ok(())
    }

    fn commit(&mut self, txid: Txid) -> Result<()> {
        debug!("start committing tx#{}", txid);

        {
            let session = self.sessions.get_mut(&txid).ok_or(Error::NoTrans)?;
            session.status_prepare()?;

            // merge emap
            self.emap.merge(&session.emap, &session.deleted)?;

            // take a new snapshot
            let snapshot = session.take_snapshot();
            snapshot.save()?;
            self.snapshots.push_back(snapshot);
            debug!("snapshot#{} is taken", txid);
        }

        // recycle retired snapshots
        self.sessions.get_mut(&txid).unwrap().status_recycle()?;
        self.recycle()?;

        // mark session as committed and remove it from session list
        self.sessions.get_mut(&txid).unwrap().status_committed()?;
        self.sessions.remove(&txid);

        debug!("tx#{} is committed", txid);

        Ok(())
    }

    fn rollback(&mut self, txid: Txid) -> Result<()> {
        debug!("start rollback back tx#{}", txid);

        let status = self.sessions.get(&txid).ok_or(Error::NoTrans)?.status;
        self.cleanup(txid, status)?;

        // reload emap
        match self.snapshots.back() {
            Some(last) => self.emap.load(last.txid)?,
            None => {
                // if no previous emap, clear all emap
                self.emap.clear();
            }
        }

        debug!("tx#{} is rolled back", txid);

        Ok(())
    }
}

impl Storage for FileStorage {
    fn exists(&self, location: &str) -> Result<bool> {
        Ok(Path::new(location).exists())
    }

    fn init(
        &mut self,
        volume_id: &Eid,
        crypto: &Crypto,
        skey: &Key,
    ) -> Result<()> {
        // create folder structure
        vio_imp::create_dir_all(&self.base)?;
        self.emap.init()?;
        self.secmgr.init()?;
        Snapshot::init(&self.base)?;
        Session::init(&self.base)?;

        // set crypto and storage key
        self.set_crypto_key(crypto, skey)?;

        // lock storage
        self.lock_storage(volume_id)?;

        debug!(
            "file storage {:?} initialised",
            self.base.file_name().unwrap()
        );
        debug!("path: {}", self.base.display());
        debug!("lock file: {}", self.lock_path.display());

        Ok(())
    }

    fn get_super_blk(&self) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        let mut file = vio_imp::File::open(&self.super_blk_path)?;
        file.read_to_end(&mut buf)?;
        Ok(buf)
    }

    fn put_super_blk(&mut self, super_blk: &[u8]) -> Result<()> {
        let mut file = vio_imp::OpenOptions::new()
            .write(true)
            .truncate(true)
            .create(true)
            .open(&self.super_blk_path)?;
        file.write_all(super_blk)?;
        Ok(())
    }

    fn open(
        &mut self,
        volume_id: &Eid,
        crypto: &Crypto,
        skey: &Key,
    ) -> Result<Txid> {
        // lock storage
        self.lock_storage(volume_id)?;

        // set crypto and storage key
        self.set_crypto_key(crypto, skey)?;

        // load session history
        let hist = Session::load_history(&self.base)?;
        if hist.is_empty() {
            return Ok(Txid::new_empty());
        }

        // do some sanity checks
        let mut completed_cnt = 0;
        let mut uncompleted_cnt = 0;
        let mut uncompleted_pos = hist.len() - 1;
        for (i, item) in hist.iter().enumerate() {
            if item.is_completed() {
                completed_cnt += 1;
            } else {
                uncompleted_cnt += 1;
                uncompleted_pos = min(i, uncompleted_pos);
            }
        }
        if completed_cnt == 0 || uncompleted_cnt > 1
            || uncompleted_pos != hist.len() - 1
        {
            return Err(Error::Corrupted);
        }

        // load snapshots
        self.snapshots.clear();
        for item in hist.iter() {
            let snapshot = if item.status == TxStatus::Committed {
                Snapshot::load(&self.base, item.txid, &self.skey, &self.crypto)?
            } else {
                Snapshot::new(
                    item.seq,
                    item.txid,
                    0,
                    Vec::new(),
                    self.base.clone(),
                    &self.skey,
                    &self.crypto,
                )
            };
            self.snapshots.push_back(snapshot);
        }

        // cleanup uncompleted session
        let last = hist.last().unwrap();
        if !last.is_completed() {
            debug!("uncompleted tx#{} found", last.txid);
            self.cleanup(last.txid, last.status)?;
        }

        // reload emap to last comitted transaction
        let last = self.snapshots.back().unwrap();
        self.emap.load(last.txid)?;

        // get seq from last committed transaction
        let last_comitted =
            hist.iter().filter(|h| h.is_completed()).last().unwrap();
        self.seq = last_comitted.seq + 1;

        debug!(
            "file storage {} opened. seq: {}, snapshots_cnt: {}, \
             last_commit: {}",
            self.base.display(),
            self.seq,
            self.snapshots.len(),
            last_comitted.txid
        );
        debug!("path: {}", self.base.display());
        debug!("lock file: {}", self.lock_path.display());

        Ok(last_comitted.txid)
    }

    fn read(
        &mut self,
        id: &Eid,
        offset: u64,
        buf: &mut [u8],
        txid: Txid,
    ) -> IoResult<usize> {
        if !txid.is_empty() {
            let session =
                map_io_err!(self.sessions.get(&txid).ok_or(Error::NoTrans))?;
            if let Some(space) = session.emap.get(id) {
                return self.secmgr.read(buf, space, offset);
            }
        }
        match self.emap.get(id) {
            Some(space) => self.secmgr.read(buf, space, offset),
            None => Err(IoError::new(
                ErrorKind::NotFound,
                Error::NoEntity.description(),
            )),
        }
    }

    fn write(
        &mut self,
        id: &Eid,
        offset: u64,
        buf: &[u8],
        txid: Txid,
    ) -> IoResult<usize> {
        let session =
            map_io_err!(self.sessions.get_mut(&txid).ok_or(Error::NoTrans))?;
        let buf_len = buf.len();
        let mut space;
        let curr = match session.emap.get(id) {
            Some(s) => Some(s.clone()),
            None => self.emap.get(id).map(|s| s.clone()),
        };

        match curr {
            Some(curr_space) => {
                if offset == 0 {
                    // overwrite existing entity, discard the old space
                    session.recycle.push(curr_space.clone());
                    space = session.alloc(buf_len);
                } else {
                    // appending to the existing entity
                    assert_eq!(offset, curr_space.len() as u64);
                    assert_eq!(txid, curr_space.txid);
                    space = curr_space.clone();

                    let end_offset = offset + buf_len as u64;
                    let ubound = align_ceil(offset as usize, BLK_SIZE) as u64;
                    let align_len = (ubound - offset) as usize;

                    // invalidate the last block of the space in cache
                    if align_len > 0 {
                        let last_span = space.spans.list.last().unwrap();
                        self.secmgr
                            .remove_cache(LocId::new(txid, last_span.end - 1));
                    }

                    if end_offset <= ubound {
                        // the last block has enough space to hold the data
                        space.set_len(end_offset as usize);
                    } else {
                        // not enough space, need to alloc extra space
                        let extra_space = session.alloc(buf_len - align_len);
                        let new_len = space.len() + align_len;
                        space.set_len(new_len);
                        space.append(&extra_space);
                    }
                }
            }
            None => {
                // new entity
                assert_eq!(offset, 0);
                space = session.alloc(buf_len);
            }
        }

        // write data to sector
        self.secmgr.write(buf, &space, offset)?;

        // update emap
        *session.emap.entry(id.clone()).or_insert(space) = space.clone();

        Ok(buf_len)
    }

    fn del(&mut self, id: &Eid, txid: Txid) -> Result<Option<Eid>> {
        let session = self.sessions.get_mut(&txid).ok_or(Error::NoTrans)?;

        if session.deleted.contains(id) {
            return Ok(None);
        }

        match session.emap.remove(id) {
            Some(space) => {
                session.deleted.insert(id.clone());
                session.recycle.push(space);
                Ok(Some(id.clone()))
            }
            None => {
                if let Some(space) = self.emap.get(id) {
                    session.deleted.insert(id.clone());
                    session.recycle.push(space.clone());
                    return Ok(Some(id.clone()));
                }
                Ok(None)
            }
        }
    }

    fn begin_trans(&mut self, txid: Txid) -> Result<()> {
        if self.sessions.contains_key(&txid) {
            return Err(Error::InTrans);
        }

        let mut session =
            Session::new(self.seq, txid, &self.base, &self.skey, &self.crypto);
        session.status_started()?;
        self.seq += 1;
        self.sessions.insert(txid, session);
        debug!("begin tx#{}", txid);
        Ok(())
    }

    fn abort_trans(&mut self, txid: Txid) -> Result<()> {
        debug!("abort tx#{}", txid);
        let status = {
            let session = self.sessions.get(&txid).ok_or(Error::NoTrans)?;
            assert!(!session.is_committing());
            session.status
        };
        self.cleanup(txid, status)?;
        debug!("tx#{} is aborted", txid);
        Ok(())
    }

    fn commit_trans(&mut self, txid: Txid) -> Result<()> {
        // all other transactions must be completed
        if self.sessions.values().any(|s| s.is_committing()) {
            return Err(Error::Uncompleted);
        }

        match self.commit(txid) {
            Ok(_) => Ok(()),
            Err(err) => {
                self.rollback(txid)?;
                Err(err)
            }
        }
    }
}

impl Drop for FileStorage {
    fn drop(&mut self) {
        if !self.lock_path.to_str().unwrap().is_empty() {
            match vio_imp::remove_file(&self.lock_path) {
                Ok(_) => {}
                Err(err) => warn!(
                    "remove lock file failed: {}, error: {}",
                    self.lock_path.display(),
                    err
                ),
            }
        }
    }
}

#[cfg(test)]
#[cfg(not(feature = "vio-test"))]
mod tests {
    extern crate tempdir;

    use std::thread;
    use std::sync::{Arc, RwLock};
    use self::tempdir::TempDir;
    use base::crypto::Crypto;
    use base::init_env;
    use trans::Eid;
    use super::*;

    fn setup() -> (FileStorage, PathBuf, TempDir) {
        init_env();
        let tmpdir = TempDir::new("zbox_test").expect("Create temp dir failed");
        let dir = tmpdir.path().to_path_buf();
        /*let dir = PathBuf::from("./tt");
        if dir.exists() {
            vio_imp::remove_dir_all(&dir).unwrap();
        }*/
        (FileStorage::new(&dir), dir, tmpdir)
    }

    fn renew_fs(
        fs: FileStorage,
        vol_id: &Eid,
        dir: &Path,
        key: &Key,
    ) -> FileStorage {
        let crypto = fs.crypto.clone();
        drop(fs);
        let mut fs = FileStorage::new(dir);
        fs.open(&vol_id, &crypto, key).unwrap();
        fs
    }

    #[test]
    fn init_open() {
        let (mut fs, dir, tmpdir) = setup();
        let crypto = Crypto::default();
        let key = Key::new_empty();
        let vol_id = Eid::new();

        fs.init(&vol_id, &crypto, &key).unwrap();
        assert!(fs.exists(fs.base.to_str().unwrap()).unwrap());
        assert_eq!(fs.open(&vol_id, &crypto, &key).unwrap_err(), Error::Opened);

        renew_fs(fs, &vol_id, &dir, &key);

        drop(tmpdir);
    }

    #[test]
    fn read_write() {
        let (mut fs, dir, _tmpdir) = setup();
        let crypto = Crypto::default();
        let key = Key::new_empty();
        let txid = Txid::from(42);
        let vol_id = Eid::new();

        fs.init(&vol_id, &crypto, &key).unwrap();

        // round #1, tx#42
        // ----------------
        fs.begin_trans(txid).unwrap();

        let id = Eid::new();
        let data = vec![1, 2, 3];
        fs.write(&id, 0, &data, txid).unwrap();
        fs.write(&id, data.len() as u64, &data, txid).unwrap();

        let id2 = Eid::new();
        let data2 = vec![41; BLK_SIZE];
        fs.write(&id2, 0, &data2, txid).unwrap();

        let id3 = Eid::new();
        let data3 = vec![42; BLK_SIZE + 42];
        fs.write(&id3, 0, &data3, txid).unwrap();

        fs.commit(txid).unwrap();

        // round #2, tx#43
        // ----------------
        let mut fs = renew_fs(fs, &vol_id, &dir, &key);
        let txid = Txid::new_empty();

        let mut dst = vec![42u8; data.len() * 2];
        assert_eq!(fs.read(&id, 0, &mut dst, txid).unwrap(), dst.len());
        assert_eq!(&dst[..data.len()], &data[..]);
        assert_eq!(&dst[data.len()..], &data[..]);

        let mut dst = vec![42u8; data.len()];
        assert_eq!(fs.read(&id, 1, &mut dst, txid).unwrap(), dst.len());
        assert_eq!(&dst[..data.len()], &[2, 3, 1]);

        let mut dst = vec![42u8; data2.len()];
        fs.read(&id2, 0, &mut dst, txid).unwrap();
        assert_eq!(&dst[..], &data2[..]);

        let mut dst = vec![42u8; data3.len()];
        fs.read(&id3, 0, &mut dst, txid).unwrap();
        assert_eq!(&dst[..], &data3[..]);

        let txid = Txid::from(43);
        fs.begin_trans(txid).unwrap();

        let data = vec![4, 5, 6];
        fs.write(&id, 0, &data, txid).unwrap();
        let mut dst = vec![42u8; data.len()];
        assert_eq!(fs.read(&id, 0, &mut dst, txid).unwrap(), dst.len());
        assert_eq!(&dst[..], &data[..]);

        fs.del(&id3, txid).unwrap();

        fs.commit(txid).unwrap();

        fs.read(&id3, 0, &mut dst, Txid::new_empty()).is_err();

        // round #3, tx#44
        // ----------------
        let mut fs = renew_fs(fs, &vol_id, &dir, &key);

        let txid = Txid::from(44);
        fs.begin_trans(txid).unwrap();

        let data = vec![7, 8, 9];
        fs.write(&id, 0, &data, txid).unwrap();
        fs.commit(txid).unwrap();

        let txid = Txid::new_empty();
        let mut dst = vec![42u8; data.len()];
        assert_eq!(fs.read(&id, 0, &mut dst, txid).unwrap(), dst.len());
        assert_eq!(&dst[..], &[7, 8, 9]);

        // round #4, tx#45
        // ----------------
        let mut fs = renew_fs(fs, &vol_id, &dir, &key);

        let txid = Txid::from(45);
        fs.begin_trans(txid).unwrap();
        let data = vec![1, 2, 3];
        fs.write(&id, 0, &data, txid).unwrap();
        fs.commit(txid).unwrap();

        // round #5
        // ----------------
        let mut fs = renew_fs(fs, &vol_id, &dir, &key);
        let txid = Txid::new_empty();

        let mut dst = vec![42u8; data.len()];
        assert_eq!(fs.read(&id, 0, &mut dst, txid).unwrap(), dst.len());
        assert_eq!(&dst[..], &[1, 2, 3]);

        let mut dst = vec![42u8; data2.len()];
        fs.read(&id2, 0, &mut dst, txid).unwrap();
        assert_eq!(&dst[..], &data2[..]);

        // round #6, tx#47
        // ----------------
        let mut fs = renew_fs(fs, &vol_id, &dir, &key);

        let txid = Txid::from(47);
        fs.begin_trans(txid).unwrap();
        let data = vec![4, 5, 6];
        fs.write(&id, 0, &data, txid).unwrap();
        fs.commit(txid).unwrap();

        let mut dst = vec![42u8; data.len()];
        assert_eq!(
            fs.read(&id, 0, &mut dst, Txid::new_empty()).unwrap(),
            dst.len()
        );
        assert_eq!(&dst[..], &[4, 5, 6]);

        // round #7, test rollback, tx#48
        // ----------------
        let mut fs = renew_fs(fs, &vol_id, &dir, &key);

        let txid = Txid::from(48);
        fs.begin_trans(txid).unwrap();
        let data = vec![1, 2, 3];
        fs.write(&id, 0, &data, txid).unwrap();
        fs.rollback(txid).unwrap();

        let mut dst = vec![42u8; data.len()];
        assert_eq!(
            fs.read(&id, 0, &mut dst, Txid::new_empty()).unwrap(),
            dst.len()
        );
        assert_eq!(&dst[..], &[4, 5, 6]);

        // round #8, test rollback, tx#49
        // ----------------
        let mut fs = renew_fs(fs, &vol_id, &dir, &key);

        let txid = Txid::from(49);
        fs.begin_trans(txid).unwrap();
        let data = vec![42u8; 4096];
        let id = Eid::new();
        fs.write(&id, 0, &data, txid).unwrap();
        let id2 = Eid::new();
        fs.write(&id2, 0, &data, txid).unwrap();
        fs.write(&id, data.len() as u64, &data, txid).unwrap();
        fs.commit(txid).unwrap();

        let mut dst = vec![0u8; data.len() * 2];
        assert_eq!(
            fs.read(&id, 0, &mut dst, Txid::new_empty()).unwrap(),
            dst.len()
        );
        assert_eq!(&dst[..data.len()], &data[..]);
        assert_eq!(&dst[data.len()..], &data[..]);
    }

    #[test]
    fn thread_read_write() {
        let (mut fs, _dir, _tmpdir) = setup();
        let crypto = Crypto::default();
        let key = Key::new_empty();
        let children_cnt = 5;
        let vol_id = Eid::new();

        fs.init(&vol_id, &crypto, &key).unwrap();
        let fs = Arc::new(RwLock::new(fs));

        let mut children = vec![];
        for i in 0..children_cnt {
            let fs = fs.clone();
            children.push(thread::spawn(move || {
                let mut fs = fs.write().unwrap();
                let txid = Txid::from(i);
                let buf = [i as u8; Eid::EID_SIZE];
                let id = Eid::from_slice(&buf);

                fs.begin_trans(txid).unwrap();
                fs.write(&id, 0, &buf, txid).unwrap();
                if i == 3 {
                    fs.rollback(txid).unwrap();
                } else {
                    fs.commit(txid).unwrap();
                }
            }));
        }
        for child in children {
            let _ = child.join();
        }

        let mut fs = fs.write().unwrap();
        let mut dst = [42u8; Eid::EID_SIZE];
        for i in 0..children_cnt {
            let buf = [i as u8; Eid::EID_SIZE];
            let id = Eid::from_slice(&buf);
            if i == 3 {
                fs.read(&id, 0, &mut dst, Txid::new_empty()).is_err();
            } else {
                assert_eq!(
                    fs.read(&id, 0, &mut dst, Txid::new_empty()).unwrap(),
                    dst.len()
                );
                assert_eq!(&dst[..], &buf[..]);
            }
        }
    }
}

#[cfg(test)]
#[cfg(feature = "vio-test")]
mod tests {
    extern crate tempdir;

    use std::fs;
    use std::cmp::max;
    use std::result;
    use std::io::Error as IoError;

    use self::tempdir::TempDir;
    use base::crypto::{Crypto, RandomSeed};
    use base::init_env;
    use trans::Eid;
    use volume::storage::file::vio::imp::{reset_random_error,
                                          turn_off_random_error,
                                          turn_on_random_error};
    use super::*;

    fn setup() -> (PathBuf, TempDir) {
        init_env();
        let tmpdir = TempDir::new("zbox_test").expect("Create temp dir failed");
        let dir = tmpdir.path().to_path_buf();
        //let dir = PathBuf::from("./tt");
        if dir.exists() {
            fs::remove_dir_all(&dir).unwrap();
        }
        (dir, tmpdir)
    }

    #[derive(Debug, Default)]
    struct VioError {
        opr_cnt: usize,
    }

    impl From<Error> for VioError {
        fn from(_err: Error) -> VioError {
            VioError::default()
        }
    }

    impl From<IoError> for VioError {
        fn from(_err: IoError) -> VioError {
            VioError::default()
        }
    }

    type VioResult<T> = result::Result<T, VioError>;

    fn do_write(fs: &mut FileStorage, i: usize) -> Result<()> {
        let txid = Txid::from(i as u64);
        let buf = vec![i as u8; max(Eid::EID_SIZE, i)];
        let id = Eid::from_slice(&buf[..Eid::EID_SIZE]);
        fs.begin_trans(txid)?;
        if let Err(err) = fs.write(&id, 0, &buf[..i], txid) {
            fs.abort_trans(txid)?;
            Err(Error::from(err))
        } else {
            fs.commit_trans(txid)
        }
    }

    fn do_read(fs: &mut FileStorage, i: usize) -> Result<()> {
        let mut buf = vec![i as u8; max(Eid::EID_SIZE, i)];
        let id = Eid::from_slice(&buf[..Eid::EID_SIZE]);
        fs.read(&id, 0, &mut buf, Txid::new_empty())?;
        assert_eq!(&buf[..i], &vec![i as u8; i][..]);
        Ok(())
    }

    fn run_round(mut fs: FileStorage) -> VioResult<()> {
        let max_opr_cnt: usize = 8; // must less than u8::MAX
        let mut opr_cnt = 0;

        // the 1st transaction cannot fail
        do_write(&mut fs, 0).unwrap();

        // read/write operations
        turn_on_random_error();
        for i in 1..max_opr_cnt + 1 {
            if let Err(_err) = do_write(&mut fs, i) {
                turn_off_random_error();
                return Err(VioError { opr_cnt });
            };
            opr_cnt = i;
            if let Err(_err) = do_read(&mut fs, i) {
                turn_off_random_error();
                return Err(VioError { opr_cnt });
            };
        }
        turn_off_random_error();

        Ok(())
    }

    fn verify_fs(
        vol_id: &Eid,
        dir: &Path,
        crypto: &Crypto,
        key: &Key,
        err: &VioError,
    ) {
        // re-open file storage
        let mut fs = FileStorage::new(dir);
        fs.open(vol_id, crypto, key).unwrap();

        // check all written entities
        for i in 0..err.opr_cnt {
            do_read(&mut fs, i).unwrap();
        }
    }

    #[test]
    fn random_io_failure() {
        // start test rounds
        for _ in 0..50 {
            let (dir, _tmpdir) = setup();
            let crypto = Crypto::default();
            let key = Key::new_empty();

            // re-seed random error generator
            let seed = RandomSeed::new();
            println!("seed: {:?}", seed);
            reset_random_error(&seed);

            let vol_id = Eid::new();

            // create and init file storage
            let mut fs = FileStorage::new(&dir);
            fs.init(&vol_id, &crypto, &key).unwrap();

            // run the test round
            // if a random IO error raised, re-open and verify the file storage
            if let Err(err) = run_round(fs) {
                // verify file storage
                verify_fs(&vol_id, &dir, &crypto, &key, &err);
            }
        }
    }

    //#[test]
    #[cfg_attr(rustfmt, rustfmt_skip)]
    #[allow(dead_code)]
    fn reproduce() {
        let (dir, _tmpdir) = setup();
        let crypto = Crypto::default();
        let key = Key::new_empty();
        let vol_id = Eid::new();
        let seed = RandomSeed::from(
            &[78, 207, 179, 62, 23, 246, 100, 163, 216, 72, 59, 70, 5, 82, 204,
            230, 163, 114, 21, 70, 237, 55, 86, 117, 142, 182, 226, 229, 186,
            152, 252, 102]
        );

        reset_random_error(&seed);

        // create and init file storage
        let mut fs = FileStorage::new(&dir);
        fs.init(&vol_id, &crypto, &key).unwrap();

        // run the test round
        // if a random IO error raised, verify the file storage
        if let Err(err) = run_round(fs) {
            // verify file storage
            verify_fs(&vol_id, &dir, &crypto, &key, &err);
        }
    }
}
