use std::ffi::{c_char, c_int};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Once};

use rusqlite::types::ValueRef;
use rusqlite::{ffi, params, Connection, OptionalExtension, ToSql};
use tokio::sync::Semaphore;

use crate::model::Role;
use crate::state::{lock, now};

/// Schema migrations in order; after applying `MIGRATIONS[i]`, `user_version` is `i + 1`.
const MIGRATIONS: [&str; 2] = [
    include_str!("../migrations/0001_init.sql"),
    include_str!("../migrations/0002_indexes.sql"),
];

/// Reader connections in use at the same time; further reads wait for a free one.
const MAX_READERS: usize = 8;

const CONNECTION_PRAGMAS: &str = "PRAGMA journal_mode=WAL;
PRAGMA synchronous=NORMAL;
PRAGMA foreign_keys=ON;
PRAGMA busy_timeout=5000;
PRAGMA journal_size_limit=67108864;";

/// Reader connections map the database file instead of copying pages through the page cache.
const READER_PRAGMAS: &str = "PRAGMA mmap_size=268435456;";

type AutoExtension = unsafe extern "C" fn(
    *mut ffi::sqlite3,
    *mut *mut c_char,
    *const ffi::sqlite3_api_routines,
) -> c_int;

static REGISTER_VEC: Once = Once::new();

/// SQLite handle: one writer connection behind a mutex plus a free list of at most [`MAX_READERS`]
/// reader connections.
#[derive(Clone)]
pub struct Db {
    writer: Arc<Mutex<Connection>>,
    readers: Arc<Mutex<Vec<Connection>>>,
    reader_permits: Arc<Semaphore>,
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
            reader_permits: Arc::new(Semaphore::new(MAX_READERS)),
            path: path.to_path_buf(),
        })
    }

    /// Runs `f` on a reader connection in a blocking task; waits while [`MAX_READERS`] reads run.
    pub async fn read<T, E>(
        &self,
        f: impl FnOnce(&Connection) -> Result<T, E> + Send + 'static,
    ) -> Result<T, E>
    where
        T: Send + 'static,
        E: From<rusqlite::Error> + Send + 'static,
    {
        let permit = match self.reader_permits.clone().acquire_owned().await {
            Ok(permit) => permit,
            Err(err) => return Err(E::from(interrupted(format!("reader pool closed: {err}")))),
        };
        let db = self.clone();
        run_blocking(move || {
            let _permit = permit;
            let pooled = lock(&db.readers).pop();
            let conn = match pooled {
                Some(conn) => conn,
                None => open_reader(&db.path)?,
            };
            let result = f(&conn);
            let mut readers = lock(&db.readers);
            if readers.len() < MAX_READERS {
                readers.push(conn);
            }
            result
        })
        .await
    }

    /// Runs `f` on the writer connection in a blocking task. Returning `Err` from inside an open
    /// transaction drops it, which rolls it back.
    pub async fn write<T, E>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T, E> + Send + 'static,
    ) -> Result<T, E>
    where
        T: Send + 'static,
        E: From<rusqlite::Error> + Send + 'static,
    {
        let db = self.clone();
        run_blocking(move || db.write_blocking(f)).await
    }

    /// Runs `f` on the writer connection on the current thread; use inside `spawn_blocking`.
    pub fn write_blocking<T, E>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T, E>,
    ) -> Result<T, E> {
        let mut conn = lock(&self.writer);
        f(&mut conn)
    }
}

async fn run_blocking<T, E>(f: impl FnOnce() -> Result<T, E> + Send + 'static) -> Result<T, E>
where
    T: Send + 'static,
    E: From<rusqlite::Error> + Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(result) => result,
        Err(err) if err.is_panic() => std::panic::resume_unwind(err.into_panic()),
        Err(err) => Err(E::from(interrupted(format!(
            "database task was cancelled: {err}"
        )))),
    }
}

fn interrupted(message: String) -> rusqlite::Error {
    rusqlite::Error::SqliteFailure(ffi::Error::new(ffi::SQLITE_INTERRUPT), Some(message))
}

/// Value of `settings[key]` as bytes (stored as BLOB or TEXT).
pub fn get_setting(conn: &Connection, key: &str) -> rusqlite::Result<Option<Vec<u8>>> {
    conn.query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| {
        Ok(match row.get_ref(0)? {
            ValueRef::Blob(bytes) | ValueRef::Text(bytes) => Some(bytes.to_vec()),
            _ => None,
        })
    })
    .optional()
    .map(Option::flatten)
}

pub fn put_setting(conn: &Connection, key: &str, value: impl ToSql) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO settings (key, value, updated_at) VALUES (?1, ?2, ?3)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        params![key, value, now()],
    )?;
    Ok(())
}

pub fn delete_setting(conn: &Connection, key: &str) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM settings WHERE key = ?1", [key])?;
    Ok(())
}

/// Whether the instance has an owner (setup is finished).
pub fn owner_exists(conn: &Connection) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM users WHERE role = ?1)",
        [Role::Owner],
        |row| row.get(0),
    )
}

/// Small helpers for building statements and reading rows.
pub mod sql {
    use rusqlite::types::{Type, ValueRef};
    use rusqlite::Row;

    /// `?, ?, …` for an `IN (…)` list of `count` values.
    pub fn placeholders(count: usize) -> String {
        vec!["?"; count].join(", ")
    }

    /// Escapes `\`, `%` and `_` for `LIKE … ESCAPE '\'`.
    pub fn escape_like(text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        for c in text.chars() {
            if matches!(c, '\\' | '%' | '_') {
                out.push('\\');
            }
            out.push(c);
        }
        out
    }

    /// Reads a non-negative INTEGER column (GitHub ids) as `u64`.
    pub fn column_u64(row: &Row<'_>, index: usize) -> rusqlite::Result<u64> {
        let value: i64 = row.get(index)?;
        u64::try_from(value).map_err(|err| {
            rusqlite::Error::FromSqlConversionFailure(index, Type::Integer, Box::new(err))
        })
    }

    /// A TEXT or BLOB column as a `String`, replacing invalid UTF-8; empty for NULL.
    pub fn text_column(row: &Row<'_>, index: usize) -> rusqlite::Result<String> {
        Ok(match row.get_ref(index)? {
            ValueRef::Blob(bytes) | ValueRef::Text(bytes) => {
                String::from_utf8_lossy(bytes).into_owned()
            }
            _ => String::new(),
        })
    }
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

fn open_reader(path: &Path) -> rusqlite::Result<Connection> {
    let conn = open_connection(path)?;
    conn.execute_batch(READER_PRAGMAS)?;
    Ok(conn)
}

/// Applies every migration newer than `PRAGMA user_version`, each in its own transaction.
fn migrate(conn: &mut Connection) -> rusqlite::Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let applied = usize::try_from(version).unwrap_or(0);
    for (index, migration) in MIGRATIONS.iter().enumerate().skip(applied) {
        let tx = conn.transaction()?;
        tx.execute_batch(migration)?;
        tx.execute_batch(&format!("PRAGMA user_version = {};", index + 1))?;
        tx.commit()?;
    }
    Ok(())
}
