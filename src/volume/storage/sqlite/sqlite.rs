use std::ffi::CString;
use std::os::raw::{c_int, c_void};
use std::ptr;
use std::thread::panicking;

use libsqlite3_sys as ffi;

use base::crypto::{Crypto, Key};
use error::{Error, Result};
use trans::Eid;
use volume::storage::Storable;
use volume::BLK_SIZE;

// check result code returned by sqlite
fn check_result(result: c_int) -> Result<()> {
    if result != ffi::SQLITE_OK {
        let err = ffi::Error::new(result);
        return Err(Error::from(err));
    }
    Ok(())
}

// reset and clean up statement
fn reset_stmt(stmt: *mut ffi::sqlite3_stmt) -> Result<()> {
    let result = unsafe { ffi::sqlite3_reset(stmt) };
    check_result(result)?;
    let result = unsafe { ffi::sqlite3_clear_bindings(stmt) };
    check_result(result)?;
    Ok(())
}

// bind integer parameter
fn bind_int(
    stmt: *mut ffi::sqlite3_stmt,
    col_idx: c_int,
    n: u64,
) -> Result<()> {
    let result = unsafe { ffi::sqlite3_bind_int(stmt, col_idx, n as c_int) };
    check_result(result)
}

// bind EID parameter
fn bind_id(
    stmt: *mut ffi::sqlite3_stmt,
    col_idx: c_int,
    id: &Eid,
) -> Result<()> {
    let id_str = CString::new(id.to_string()).unwrap();
    let result = unsafe {
        ffi::sqlite3_bind_text(
            stmt,
            col_idx,
            id_str.as_ptr(),
            -1,
            ffi::SQLITE_STATIC(),
        )
    };
    check_result(result)
}

// bind blob parameter
fn bind_blob(
    stmt: *mut ffi::sqlite3_stmt,
    col_idx: c_int,
    data: &[u8],
) -> Result<()> {
    let result = unsafe {
        ffi::sqlite3_bind_blob(
            stmt,
            col_idx,
            data.as_ptr() as *const c_void,
            data.len() as c_int,
            ffi::SQLITE_STATIC(),
        )
    };
    check_result(result)
}

// run DML statement, such as INSERT and DELETE
fn run_dml(stmt: *mut ffi::sqlite3_stmt) -> Result<()> {
    let result = unsafe { ffi::sqlite3_step(stmt) };
    match result {
        ffi::SQLITE_DONE => Ok(()),
        _ => Err(Error::from(ffi::Error::new(result))),
    }
}

// run SELECT statement on a blob column
fn run_select_blob(stmt: *mut ffi::sqlite3_stmt) -> Result<Vec<u8>> {
    let result = unsafe { ffi::sqlite3_step(stmt) };
    match result {
        ffi::SQLITE_ROW => {
            //  get data and data size
            let (data, data_len) = unsafe {
                (
                    ffi::sqlite3_column_blob(stmt, 0),
                    ffi::sqlite3_column_bytes(stmt, 0) as usize,
                )
            };

            // copy data to vec and return it
            let mut ret = vec![0u8; data_len];
            unsafe {
                ptr::copy_nonoverlapping(
                    data,
                    (&mut ret).as_mut_ptr() as *mut c_void,
                    data_len,
                );
            }
            Ok(ret)
        }
        ffi::SQLITE_DONE => Err(Error::NotFound),
        _ => Err(Error::from(ffi::Error::new(result))),
    }
}

/// Sqlite Storage
#[derive(Debug)]
pub struct SqliteStorage {
    filename: CString,
    db: *mut ffi::sqlite3,
    stmts: Vec<*mut ffi::sqlite3_stmt>,
}

impl SqliteStorage {
    // table name constants
    const TBL_SUPER_BLOCK: &'static str = "super_block";
    const TBL_ADDRESSES: &'static str = "addresses";
    const TBL_BLOCKS: &'static str = "blocks";

    pub fn new(filename: &str) -> Self {
        SqliteStorage {
            filename: CString::new(filename).unwrap(),
            db: ptr::null_mut(),
            stmts: Vec::new(),
        }
    }

    fn connect(&mut self, flags: c_int) -> Result<()> {
        let result = unsafe {
            ffi::sqlite3_open_v2(
                self.filename.as_ptr(),
                &mut self.db,
                flags,
                ptr::null(),
            )
        };
        if result != ffi::SQLITE_OK {
            let err = ffi::Error::new(result);
            if !self.db.is_null() {
                unsafe { ffi::sqlite3_close(self.db) };
                self.db = ptr::null_mut();
            }
            return Err(Error::from(err));
        }

        Ok(())
    }

    // prepare one sql statement
    fn prepare_sql(&mut self, sql: String) -> Result<()> {
        let mut stmt = ptr::null_mut();
        let sql = CString::new(sql).unwrap();
        let result = unsafe {
            ffi::sqlite3_prepare_v2(
                self.db,
                sql.as_ptr(),
                -1,
                &mut stmt,
                ptr::null_mut(),
            )
        };
        check_result(result)?;
        self.stmts.push(stmt);
        Ok(())
    }

    // prepare and cache all sql statements
    fn prepare_stmts(&mut self) -> Result<()> {
        // super block sql
        self.prepare_sql(format!(
            "
            SELECT data FROM {} WHERE suffix = ?
        ",
            Self::TBL_SUPER_BLOCK
        ))?;
        self.prepare_sql(format!(
            "
            INSERT INTO {}(suffix, data) VALUES (?, ?)
        ",
            Self::TBL_SUPER_BLOCK
        ))?;

        // addresses sql
        self.prepare_sql(format!(
            "
            SELECT data FROM {} WHERE id = ?
        ",
            Self::TBL_ADDRESSES
        ))?;
        self.prepare_sql(format!(
            "
            INSERT OR REPLACE INTO {}(id, data) VALUES (?, ?)
        ",
            Self::TBL_ADDRESSES
        ))?;
        self.prepare_sql(format!(
            "
            DELETE FROM {} WHERE id = ?
        ",
            Self::TBL_ADDRESSES
        ))?;

        // blocks sql
        self.prepare_sql(format!(
            "
            SELECT data FROM {} WHERE blk_idx = ?
        ",
            Self::TBL_BLOCKS
        ))?;
        self.prepare_sql(format!(
            "
            INSERT INTO {}(blk_idx, data) VALUES (?, ?)
        ",
            Self::TBL_BLOCKS
        ))?;
        self.prepare_sql(format!(
            "
            DELETE FROM {} WHERE blk_idx = ?
        ",
            Self::TBL_BLOCKS
        ))?;

        Ok(())
    }
}

impl Storable for SqliteStorage {
    fn exists(&self) -> Result<bool> {
        let mut db: *mut ffi::sqlite3 = ptr::null_mut();
        let result = unsafe {
            ffi::sqlite3_open_v2(
                self.filename.as_ptr(),
                &mut db,
                ffi::SQLITE_OPEN_READONLY,
                ptr::null(),
            )
        };
        if !db.is_null() {
            unsafe { ffi::sqlite3_close(db) };
        }
        Ok(result == ffi::SQLITE_OK)
    }

    fn init(&mut self, _crypto: Crypto, _key: Key) -> Result<()> {
        // open or create db
        self.connect(
            ffi::SQLITE_OPEN_READWRITE
                | ffi::SQLITE_OPEN_CREATE
                | ffi::SQLITE_OPEN_FULLMUTEX,
        )?;

        // create tables
        let sql = format!(
            "
            CREATE TABLE {} (
                suffix      INTEGER PRIMARY KEY,
                data        BLOB
            );
            CREATE TABLE {} (
                id          TEXT PRIMARY KEY,
                data        BLOB
            );
            CREATE TABLE {} (
                blk_idx     INTEGER PRIMARY KEY,
                data        BLOB
            );
        ",
            Self::TBL_SUPER_BLOCK,
            Self::TBL_ADDRESSES,
            Self::TBL_BLOCKS
        );
        let sql = CString::new(sql).unwrap();
        let result = unsafe {
            ffi::sqlite3_exec(
                self.db,
                sql.as_ptr(),
                None,
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        check_result(result)?;

        // prepare statements
        self.prepare_stmts()?;

        Ok(())
    }

    fn open(&mut self, _crypto: Crypto, _key: Key) -> Result<()> {
        // open db
        self.connect(ffi::SQLITE_OPEN_READWRITE | ffi::SQLITE_OPEN_FULLMUTEX)?;

        // prepare statements
        self.prepare_stmts()
    }

    fn get_super_block(&mut self, suffix: u64) -> Result<Vec<u8>> {
        let stmt = self.stmts[0];
        reset_stmt(stmt)?;

        // bind parameters and run sql
        bind_int(stmt, 1, suffix).and(run_select_blob(stmt))
    }

    fn put_super_block(&mut self, super_blk: &[u8], suffix: u64) -> Result<()> {
        let stmt = self.stmts[1];
        reset_stmt(stmt)?;

        // bind parameters and run sql
        bind_int(stmt, 1, suffix)
            .and(bind_blob(stmt, 2, super_blk))
            .and(run_dml(stmt))
    }

    fn get_addr(&mut self, id: &Eid) -> Result<Vec<u8>> {
        let stmt = self.stmts[2];
        reset_stmt(stmt)?;

        // bind parameters and run sql
        bind_id(stmt, 1, id).and(run_select_blob(stmt))
    }

    fn put_addr(&mut self, id: &Eid, addr: &[u8]) -> Result<()> {
        let stmt = self.stmts[3];
        reset_stmt(stmt)?;

        // bind parameters and run sql
        bind_id(stmt, 1, id)
            .and(bind_blob(stmt, 2, addr))
            .and(run_dml(stmt))
    }

    fn del_addr(&mut self, id: &Eid) -> Result<()> {
        let stmt = self.stmts[4];
        reset_stmt(stmt)?;

        // bind parameters and run sql
        bind_id(stmt, 1, id).and(run_dml(stmt))
    }

    fn get_blocks(
        &mut self,
        dst: &mut [u8],
        start_idx: u64,
        cnt: usize,
    ) -> Result<()> {
        assert_eq!(dst.len(), BLK_SIZE * cnt);

        let stmt = self.stmts[5];

        let mut read = 0;
        for blk_idx in start_idx..start_idx + cnt as u64 {
            // reset statement and binding
            reset_stmt(stmt)?;

            // bind parameters and run sql
            let blk = bind_int(stmt, 1, blk_idx).and(run_select_blob(stmt))?;
            assert_eq!(blk.len(), BLK_SIZE);
            dst[read..read + BLK_SIZE].copy_from_slice(&blk);
            read += BLK_SIZE;
        }

        Ok(())
    }

    fn put_blocks(
        &mut self,
        start_idx: u64,
        cnt: usize,
        mut blks: &[u8],
    ) -> Result<()> {
        assert_eq!(blks.len(), BLK_SIZE * cnt);

        let stmt = self.stmts[6];

        for blk_idx in start_idx..start_idx + cnt as u64 {
            // reset statement and binding
            reset_stmt(stmt)?;

            // bind parameters and run sql
            bind_int(stmt, 1, blk_idx)
                .and(bind_blob(stmt, 2, &blks[..BLK_SIZE]))
                .and(run_dml(stmt))?;

            blks = &blks[BLK_SIZE..];
        }

        Ok(())
    }

    fn del_blocks(&mut self, start_idx: u64, cnt: usize) -> Result<()> {
        let stmt = self.stmts[7];

        for blk_idx in start_idx..start_idx + cnt as u64 {
            // reset statement and binding
            reset_stmt(stmt)?;

            // bind parameters and run sql
            bind_int(stmt, 1, blk_idx).and(run_dml(stmt))?;
        }

        Ok(())
    }
}

impl Drop for SqliteStorage {
    fn drop(&mut self) {
        unsafe {
            for stmt in self.stmts.iter() {
                ffi::sqlite3_finalize(*stmt);
            }
        }
        let result = unsafe { ffi::sqlite3_close(self.db) };
        if result != ffi::SQLITE_OK {
            if panicking() {
                eprintln!("Error while closing SQLite connection: {}", result);
            } else {
                panic!("Error while closing SQLite connection: {}", result);
            }
        }
    }
}

unsafe impl Send for SqliteStorage {}
unsafe impl Sync for SqliteStorage {}

#[cfg(test)]
mod tests {
    extern crate tempdir;

    use self::tempdir::TempDir;

    use super::*;

    use base::init_env;

    #[test]
    fn sqlite_storage() {
        init_env();
        let tmpdir = TempDir::new("zbox_test").expect("Create temp dir failed");
        let dir = tmpdir.path().join("storage.db");
        let mut ss = SqliteStorage::new(dir.to_str().unwrap());

        ss.init(Crypto::default(), Key::new_empty()).unwrap();

        let id = Eid::new();
        let buf = vec![1, 2, 3];
        let blks = vec![42u8; BLK_SIZE * 3];
        let mut dst = vec![0u8; BLK_SIZE * 3];

        // super block
        ss.put_super_block(&buf, 0).unwrap();
        let s = ss.get_super_block(0).unwrap();
        assert_eq!(&s[..], &buf[..]);

        // address
        ss.put_addr(&id, &buf).unwrap();
        let s = ss.get_addr(&id).unwrap();
        assert_eq!(&s[..], &buf[..]);
        ss.del_addr(&id).unwrap();
        assert_eq!(ss.get_addr(&id).unwrap_err(), Error::NotFound);

        // block
        ss.put_blocks(0, 3, &blks).unwrap();
        ss.get_blocks(&mut dst, 0, 3).unwrap();
        assert_eq!(&dst[..], &blks[..]);
        ss.del_blocks(1, 2).unwrap();
        assert_eq!(ss.get_blocks(&mut dst, 0, 3).unwrap_err(), Error::NotFound);
        assert_eq!(
            ss.get_blocks(&mut dst[..BLK_SIZE], 1, 1).unwrap_err(),
            Error::NotFound
        );
        assert_eq!(
            ss.get_blocks(&mut dst[..BLK_SIZE], 2, 1).unwrap_err(),
            Error::NotFound
        );

        // re-open
        drop(ss);
        let mut ss = SqliteStorage::new(dir.to_str().unwrap());
        ss.open(Crypto::default(), Key::new_empty()).unwrap();

        ss.get_blocks(&mut dst[..BLK_SIZE], 0, 1).unwrap();
        assert_eq!(&dst[..BLK_SIZE], &blks[..BLK_SIZE]);
        assert_eq!(
            ss.get_blocks(&mut dst[..BLK_SIZE], 1, 1).unwrap_err(),
            Error::NotFound
        );
        assert_eq!(
            ss.get_blocks(&mut dst[..BLK_SIZE], 2, 1).unwrap_err(),
            Error::NotFound
        );
    }
}
