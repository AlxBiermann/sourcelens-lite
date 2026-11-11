use std::ffi::{c_char, c_int};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, Once};

use rusqlite::{ffi, Connection};

const MIGRATION_0001: &str = include_str!("../migrations/0001_init.sql");
const SCHEMA_VERSION: i64 = 1;

const CONNECTION_PRAGMAS: &str = "PRAGMA journal_mode=WAL;
PRAGMA synchronous=NORMAL;
PRAGMA foreign_keys=ON;
PRAGMA busy_timeout=5000;";

type AutoExtension = unsafe extern "C" fn(
    *mut ffi::sqlite3,
    *mut *mut c_char,
    *const ffi::sqlite3_api_routines,
) -> c_int;

static REGISTER_VEC: Once = Once::new();

/// SQLite handle: one writer connection behind a mutex plus a free list of reader connections.
#[derive(Clone)]
pub struct Db {
    writer: Arc<Mutex<Connection>>,
    readers: Arc<Mutex<Vec<Connection>>>,
    path: PathBuf,
}

impl Db {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        register_sqlite_vec();
        let mut writer = open_connection(path)?;
        migrate(&mut writer)?;
        Ok(Self {
            writer: Arc::new(Mutex::new(writer)),
            readers: Arc::new(Mutex::new(Vec::new())),
            path: path.to_path_buf(),
        })
    }

    pub async fn read<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Connection) -> rusqlite::Result<T> + Send + 'static,
    ) -> rusqlite::Result<T> {
        let db = self.clone();
        run_blocking(move || {
            let pooled = lock(&db.readers).pop();
            let conn = match pooled {
                Some(conn) => conn,
                None => open_connection(&db.path)?,
            };
            let result = f(&conn);
            lock(&db.readers).push(conn);
            result
        })
        .await
    }

    pub async fn write<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection) -> rusqlite::Result<T> + Send + 'static,
    ) -> rusqlite::Result<T> {
        let db = self.clone();
        run_blocking(move || db.write_blocking(f)).await
    }

    /// Runs `f` on the writer connection on the current thread; use inside `spawn_blocking`.
    pub fn write_blocking<T>(
        &self,
        f: impl FnOnce(&mut Connection) -> rusqlite::Result<T>,
    ) -> rusqlite::Result<T> {
        let mut conn = lock(&self.writer);
        f(&mut conn)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

async fn run_blocking<T: Send + 'static>(
    f: impl FnOnce() -> rusqlite::Result<T> + Send + 'static,
) -> rusqlite::Result<T> {
    match tokio::task::spawn_blocking(f).await {
        Ok(result) => result,
        Err(err) if err.is_panic() => std::panic::resume_unwind(err.into_panic()),
        Err(err) => Err(rusqlite::Error::SqliteFailure(
            ffi::Error::new(ffi::SQLITE_INTERRUPT),
            Some(format!("database task was cancelled: {err}")),
        )),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn register_sqlite_vec() {
    REGISTER_VEC.call_once(|| {
        // SAFETY: sqlite3_vec_init is the sqlite-vec extension entry point, whose real C signature is the
        // auto-extension signature; this is the registration pattern documented by the sqlite-vec crate.
        unsafe {
            ffi::sqlite3_auto_extension(Some(std::mem::transmute::<*const (), AutoExtension>(
                sqlite_vec::sqlite3_vec_init as *const (),
            )));
        }
    });
}

fn open_connection(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.execute_batch(CONNECTION_PRAGMAS)?;
    Ok(conn)
}

fn migrate(conn: &mut Connection) -> rusqlite::Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version >= SCHEMA_VERSION {
        return Ok(());
    }
    let tx = conn.transaction()?;
    tx.execute_batch(MIGRATION_0001)?;
    tx.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))?;
    tx.commit()
}
