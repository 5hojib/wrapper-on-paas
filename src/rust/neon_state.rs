use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use postgres::{Client, NoTls};
use postgres_native_tls::MakeTlsConnector;
use native_tls::TlsConnector;
use crate::protocol;
use crate::worker::Worker;
use serde_json::Value;
use tar::{Archive, Builder};

static LAST_HASH: AtomicU64 = AtomicU64::new(0);
static NEON_LOCK: Mutex<()> = Mutex::new(());

fn neon_url() -> Option<String> {
    env::var("NEON_DATABASE_URL")
        .ok()
        .filter(|v| !v.is_empty())
}

fn state_key() -> String {
    env::var("WRAPPER_STATE_KEY")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "default".to_string())
}

fn base_dir() -> String {
    env::var("WRAPPER_BASE_DIR")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "/data/data/com.apple.android.music/files".to_string())
}

fn connect() -> io::Result<Client> {
    let url = neon_url().ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "NEON_DATABASE_URL unset"))?;
    if url.contains("sslmode=require") || url.contains("ssl=true") {
        let connector = TlsConnector::new()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        let tls = MakeTlsConnector::new(connector);
        Client::connect(&url, tls).map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))
    } else {
        Client::connect(&url, NoTls).map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))
    }
}

pub fn restore_state() -> io::Result<()> {
    let Some(_url) = neon_url() else { return Ok(()); };
    let mut client = connect()?;
    client
        .batch_execute(
            "CREATE TABLE IF NOT EXISTS wrapper_state (key TEXT PRIMARY KEY, data BYTEA, updated_at TIMESTAMPTZ DEFAULT now())",
        )
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;

    let key = state_key();
    let row = client
        .query_opt("SELECT data FROM wrapper_state WHERE key = $1", &[&key])
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
    let Some(row) = row else { return Ok(()) };
    let data: Vec<u8> = row.get(0);
    if data.is_empty() {
        return Ok(());
    }

    let dir = base_dir();
    fs::create_dir_all(&dir)?;
    let decoder = GzDecoder::new(&data[..]);
    let mut archive = Archive::new(decoder);
    archive.unpack(&dir).map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
    eprintln!("neon: restored auth state from database");
    Ok(())
}

fn build_tarball() -> io::Result<Vec<u8>> {
    let dir = base_dir();
    let mpl_db = Path::new(&dir).join("mpl_db");
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    {
        let mut builder = Builder::new(&mut encoder);
        if mpl_db.exists() {
            builder.append_dir_all("mpl_db", &mpl_db)?;
        }
        builder.finish()?;
    }
    encoder.finish()
}

pub fn backup_state(worker: &Worker) -> io::Result<()> {
    let Some(_url) = neon_url() else { return Ok(()); };
    // Only back up when authenticated.
    let resp = match worker.request_json(crate::protocol::OP_ME, Value::Null) {
        Ok(r) => r,
        Err(_) => return Ok(()),
    };
    let body = String::from_utf8_lossy(&resp.body);
    if !body.contains("authenticated") {
        return Ok(());
    }

    let data = build_tarball()?;
    if data.is_empty() {
        return Ok(());
    }

    // Cheap change detection.
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    data.hash(&mut hasher);
    let hash = hasher.finish();
    if hash == LAST_HASH.load(Ordering::Relaxed) {
        return Ok(());
    }

    let _lock = NEON_LOCK.lock().unwrap();
    let mut client = connect()?;
    client
        .batch_execute(
            "CREATE TABLE IF NOT EXISTS wrapper_state (key TEXT PRIMARY KEY, data BYTEA, updated_at TIMESTAMPTZ DEFAULT now())",
        )
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
    let key = state_key();
    client
        .execute(
            "INSERT INTO wrapper_state (key, data, updated_at) VALUES ($1, $2, now()) ON CONFLICT (key) DO UPDATE SET data = EXCLUDED.data, updated_at = now()",
            &[&key, &data],
        )
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
    LAST_HASH.store(hash, Ordering::Relaxed);
    eprintln!("neon: backed up auth state ({} bytes)", data.len());
    Ok(())
}

pub fn spawn_backup_loop(worker: std::sync::Arc<Worker>) {
    if neon_url().is_none() {
        return;
    }
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(60));
        let _ = backup_state(&worker);
    });
}