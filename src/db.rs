//! Event persistence and querying
use crate::error::Result;
use crate::protocol::Event;
use crate::protocol::Subscription;
use governor::clock::Clock;
use governor::{Quota, RateLimiter};
use hex;
use log::*;
use rusqlite::params;
use rusqlite::Connection;
use rusqlite::OpenFlags;
//use std::num::NonZeroU32;
use crate::config::SETTINGS;
use std::path::Path;
use std::thread;
use std::time::Instant;
use tokio::task;

use std::str::FromStr;

use bitcoin_hashes::{hex::ToHex, Hash};

/// Database file
const DB_FILE: &str = "nostr.db";

/// Startup DB Pragmas
const STARTUP_SQL: &str = r##"
PRAGMA main.synchronous=NORMAL;
PRAGMA foreign_keys = ON;
pragma mmap_size = 536870912; -- 512MB of mmap
"##;

/// Schema definition
const INIT_SQL: &str = r##"
-- Database settings
PRAGMA encoding = "UTF-8";
PRAGMA journal_mode=WAL;
PRAGMA main.synchronous=NORMAL;
PRAGMA foreign_keys = ON;
PRAGMA application_id = 1654008667;
PRAGMA user_version = 2;

-- Event Table
CREATE TABLE IF NOT EXISTS event (
id INTEGER PRIMARY KEY,
event_hash BLOB NOT NULL, -- 4-byte hash
first_seen INTEGER NOT NULL, -- when the event was first seen (not authored!) (seconds since 1970)
created_at INTEGER NOT NULL, -- when the event was authored
author BLOB NOT NULL, -- author pubkey
kind INTEGER NOT NULL, -- event kind
hidden INTEGER, -- relevant for queries
content TEXT NOT NULL -- serialized json of event object
);

-- Event Indexes
CREATE UNIQUE INDEX IF NOT EXISTS event_hash_index ON event(event_hash);
CREATE INDEX IF NOT EXISTS created_at_index ON event(created_at);
CREATE INDEX IF NOT EXISTS author_index ON event(author);
CREATE INDEX IF NOT EXISTS kind_index ON event(kind);

-- Event References Table
CREATE TABLE IF NOT EXISTS event_ref (
id INTEGER PRIMARY KEY,
event_id INTEGER NOT NULL, -- an event ID that contains an #e tag.
referenced_event BLOB NOT NULL, -- the event that is referenced.
FOREIGN KEY(event_id) REFERENCES event(id) ON UPDATE CASCADE ON DELETE CASCADE
);

-- Event References Index
CREATE INDEX IF NOT EXISTS event_ref_index ON event_ref(referenced_event);

-- Pubkey References Table
CREATE TABLE IF NOT EXISTS pubkey_ref (
id INTEGER PRIMARY KEY,
event_id INTEGER NOT NULL, -- an event ID that contains an #p tag.
referenced_pubkey BLOB NOT NULL, -- the pubkey that is referenced.
FOREIGN KEY(event_id) REFERENCES event(id) ON UPDATE RESTRICT ON DELETE CASCADE
);

-- Pubkey References Index
CREATE INDEX IF NOT EXISTS pubkey_ref_index ON pubkey_ref(referenced_pubkey);
"##;

/// Upgrade DB to latest version, and execute pragma settings
pub fn upgrade_db(conn: &mut Connection) -> Result<()> {
    // check the version.
    let curr_version = db_version(conn)?;
    info!("DB version = {:?}", curr_version);

    // initialize from scratch
    if curr_version == 0 {
        match conn.execute_batch(INIT_SQL) {
            Ok(()) => info!("database pragma/schema initialized to v2, and ready"),
            Err(err) => {
                error!("update failed: {}", err);
                panic!("database could not be initialized");
            }
        }
    } else if curr_version == 1 {
        // only change is adding a hidden column to events.
        let upgrade_sql = r##"
ALTER TABLE event ADD hidden INTEGER;
UPDATE event SET hidden=FALSE;
PRAGMA user_version = 2;
"##;
        match conn.execute_batch(upgrade_sql) {
            Ok(()) => info!("database schema upgraded v1 -> v2"),
            Err(err) => {
                error!("update failed: {}", err);
                panic!("database could not be upgraded");
            }
        }
    } else if curr_version == 2 {
        debug!("Database version was already current");
    } else if curr_version > 2 {
        panic!("Database version is newer than supported by this executable");
    }
    // Setup PRAGMA
    conn.execute_batch(STARTUP_SQL)?;
    Ok(())
}

/// Spawn a database writer that persists events to the SQLite store.
pub async fn db_writer(
    mut event_rx: tokio::sync::mpsc::Receiver<Event>,
    bcast_tx: tokio::sync::broadcast::Sender<Event>,
    mut shutdown: tokio::sync::broadcast::Receiver<()>,
) -> tokio::task::JoinHandle<Result<()>> {
    task::spawn_blocking(move || {
        // get database configuration settings
        let config = SETTINGS.read().unwrap();
        let db_dir = &config.database.data_directory;
        let full_path = Path::new(db_dir).join(DB_FILE);
        // create a connection
        let mut conn = Connection::open_with_flags(
            &full_path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        )?;
        info!("opened database {:?} for writing", full_path);
        upgrade_db(&mut conn)?;
        // get rate limit settings
        let rps_setting = config.limits.messages_per_sec;
        let mut most_recent_rate_limit = Instant::now();
        let mut lim_opt = None;
        let clock = governor::clock::QuantaClock::default();
        if let Some(rps) = rps_setting {
            if rps > 0 {
                info!("Enabling rate limits for event creation ({}/sec)", rps);
                let quota = core::num::NonZeroU32::new(rps * 60).unwrap();
                lim_opt = Some(RateLimiter::direct(Quota::per_minute(quota)));
            }
        }
        loop {
            if shutdown.try_recv().is_ok() {
                info!("shutting down database writer");
                break;
            }
            // call blocking read on channel
            let next_event = event_rx.blocking_recv();
            // if the channel has closed, we will never get work
            if next_event.is_none() {
                break;
            }
            let mut event_write = false;
            let event = next_event.unwrap();
            let start = Instant::now();
            match write_event(&mut conn, &event) {
                Ok(updated) => {
                    if updated == 0 {
                        debug!("ignoring duplicate event");
                    } else {
                        info!(
                            "persisted event: {} in {:?}",
                            event.get_short_event_id(),
                            start.elapsed()
                        );
                        event_write = true;
                        // send this out to all clients
                        bcast_tx.send(event.clone()).ok();
                    }
                }
                Err(err) => {
                    warn!("event insert failed: {}", err);
                }
            }
            // use rate limit, if defined, and if an event was actually written.
            if event_write {
                if let Some(ref lim) = lim_opt {
                    if let Err(n) = lim.check() {
                        let wait_for = n.wait_time_from(clock.now());
                        // check if we have recently logged rate
                        // limits, but print out a message only once
                        // per second.
                        if most_recent_rate_limit.elapsed().as_secs() > 1 {
                            warn!(
                                "rate limit reached for event creation (sleep for {:?})",
                                wait_for
                            );
                            // reset last rate limit message
                            most_recent_rate_limit = Instant::now();
                        }
                        // block event writes, allowing them to queue up
                        thread::sleep(wait_for);
                        continue;
                    }
                }
            }
        }
        conn.close().ok();
        info!("database connection closed");
        Ok(())
    })
}

pub fn db_version(conn: &mut Connection) -> Result<usize> {
    let query = "PRAGMA user_version;";
    let curr_version = conn.query_row(query, [], |row| row.get(0))?;
    Ok(curr_version)
}

/// Persist an event to the database.
pub fn write_event(conn: &mut Connection, e: &Event) -> Result<usize> {
    // start transaction
    let tx = conn.transaction()?;
    // get relevant fields from event and convert to blobs.
    let id_blob = e.id.as_inner().to_vec();
    let pubkey_blob = e.pubkey.serialize().to_vec();
    let event_str = serde_json::to_string(&e).ok();
    let event_kind = serde_json::to_value(&e.kind)?
        .as_u64()
        .expect("expect a kind");
    // ignore if the event hash is a duplicate.
    let ins_count = tx.execute(
        "INSERT OR IGNORE INTO event (event_hash, created_at, kind, author, content, first_seen, hidden) VALUES (?1, ?2, ?3, ?4, ?5, strftime('%s','now'), FALSE);",
        params![id_blob, e.created_at, event_kind, pubkey_blob, event_str]
    )?;
    if ins_count == 0 {
        // if the event was a duplicate, no need to insert event or
        // pubkey references.
        return Ok(ins_count);
    }
    // remember primary key of the event most recently inserted.
    let ev_id = tx.last_insert_rowid();
    // add all event tags into the event_ref table
    let etags = e.clone().get_event_tags().unwrap();
    if !etags.is_empty() {
        for etag in etags.iter() {
            tx.execute(
                "INSERT OR IGNORE INTO event_ref (event_id, referenced_event) VALUES (?1, ?2)",
                params![ev_id, hex::decode(&etag.to_string().as_bytes()).ok()],
            )?;
        }
    }
    // add all event tags into the pubkey_ref table
    let ptags = e.clone().get_pubkey_tags().unwrap();
    if !ptags.is_empty() {
        for ptag in ptags.iter() {
            tx.execute(
                "INSERT OR IGNORE INTO pubkey_ref (event_id, referenced_pubkey) VALUES (?1, ?2)",
                params![ev_id, hex::decode(&ptag.to_string().as_bytes()).ok()],
            )?;
        }
    }
    // if this event is for a metadata update, hide every other kind=0
    // event from the same author that was issued earlier than this.
    if event_kind == 0 {
        let update_count = tx.execute(
            "UPDATE event SET hidden=TRUE WHERE id!=? AND kind=0 AND author=? AND created_at <= ? and hidden!=TRUE",
            params![ev_id, hex::decode(&e.pubkey.to_string()).ok(), e.created_at],
        )?;
        if update_count > 0 {
            info!("hid {} older metadata events", update_count);
        }
    }
    // if this event is for a contact update, hide every other kind=3
    // event from the same author that was issued earlier than this.
    if event_kind == 3 {
        let update_count = tx.execute(
            "UPDATE event SET hidden=TRUE WHERE id!=? AND kind=3 AND author=? AND created_at <= ? and hidden!=TRUE",
            params![ev_id, hex::decode(&e.pubkey.to_string()).ok(), e.created_at],
        )?;
        if update_count > 0 {
            info!("hid {} older contact events", update_count);
        }
    }
    tx.commit()?;
    Ok(ins_count)
}

/// Event resulting from a specific subscription request
#[derive(PartialEq, Debug, Clone)]
pub struct QueryResult {
    /// Subscription identifier
    pub sub_id: String,
    /// Serialized event
    pub event: Event,
}

/// Check if a string contains only hex characters.
fn is_hex(s: &str) -> bool {
    s.chars().all(|x| char::is_ascii_hexdigit(&x))
}

/// Create a dynamic SQL query string from a subscription.
fn query_from_sub(sub: &Subscription) -> String {
    // build a dynamic SQL query.  all user-input is either an integer
    // (sqli-safe), or a string that is filtered to only contain
    // hexadecimal characters.
    let mut query =
        "SELECT DISTINCT(e.content) FROM event e LEFT JOIN event_ref er ON e.id=er.event_id LEFT JOIN pubkey_ref pr ON e.id=pr.event_id "
            .to_owned();
    // for every filter in the subscription, generate a where clause
    let mut filter_clauses: Vec<String> = Vec::new();
    for f in sub.get_filters().iter() {
        // individual filter components
        let mut filter_components: Vec<String> = Vec::new();
        // Query for "authors"
        if f.authors.is_some() {
            let authors_escaped: Vec<String> = f
                .authors
                .as_ref()
                .unwrap()
                .iter()
                .filter(|&x| is_hex(&x.to_hex()))
                .map(|x| format!("x'{}'", x))
                .collect();
            let authors_clause = format!("author IN ({})", authors_escaped.join(", "));
            filter_components.push(authors_clause);
        }
        // Query for Kind
        if let Some(ks) = &f.kinds {
            // kind is number, no escaping needed
            let str_kinds: Vec<String> = ks.iter().map(|x| x.to_string()).collect();
            let kind_clause = format!("kind IN ({})", str_kinds.join(", "));
            filter_components.push(kind_clause);
        }
        // Query for event
        if f.ids.is_some() {
            let ids_escaped: Vec<String> = f
                .ids
                .as_ref()
                .unwrap()
                .iter()
                .filter(|&x| is_hex(&x.to_hex()))
                .map(|x| format!("x'{}'", x))
                .collect();
            let id_clause = format!("event_hash IN ({})", ids_escaped.join(", "));
            filter_components.push(id_clause);
        }
        // Query for referenced event
        if f.events.is_some() {
            let events_escaped: Vec<String> = f
                .events
                .as_ref()
                .unwrap()
                .iter()
                .filter(|&x| is_hex(&x.to_hex()))
                .map(|x| format!("x'{}'", x))
                .collect();
            let events_clause = format!("referenced_event IN ({})", events_escaped.join(", "));
            filter_components.push(events_clause);
        }
        // Query for referenced pubkey
        if f.pubkeys.is_some() {
            let pubkeys_escaped: Vec<String> = f
                .pubkeys
                .as_ref()
                .unwrap()
                .iter()
                .filter(|&x| is_hex(&x.to_hex()))
                .map(|x| format!("x'{}'", x))
                .collect();
            let pubkeys_clause = format!("referenced_pubkey IN ({})", pubkeys_escaped.join(", "));
            filter_components.push(pubkeys_clause);
        }

        // Query for timestamp
        if f.since.is_some() {
            let created_clause = format!("created_at > {}", f.since.unwrap());
            filter_components.push(created_clause);
        }
        // Query for timestamp
        if f.until.is_some() {
            let until_clause = format!("created_at < {}", f.until.unwrap());
            filter_components.push(until_clause);
        }

        // combine all clauses, and add to filter_clauses
        if !filter_components.is_empty() {
            let mut fc = "( ".to_owned();
            fc.push_str(&filter_components.join(" AND "));
            fc.push_str(" )");
            filter_clauses.push(fc);
        } else {
            // never display hidden events
            filter_clauses.push("hidden!=TRUE".to_owned());
        }
    }

    // combine all filters with OR clauses, if any exist
    if !filter_clauses.is_empty() {
        query.push_str(" WHERE ");
        query.push_str(&filter_clauses.join(" OR "));
    }
    // add order clause
    query.push_str(" ORDER BY created_at ASC");
    debug!("query string: {}", query);
    query
}

/// Perform a database query using a subscription.
///
/// The [`Subscription`] is converted into a SQL query.  Each result
/// is published on the `query_tx` channel as it is returned.  If a
/// message becomes available on the `abandon_query_rx` channel, the
/// query is immediately aborted.
pub async fn db_query(
    sub: Subscription,
    query_tx: tokio::sync::mpsc::Sender<QueryResult>,
    mut abandon_query_rx: tokio::sync::oneshot::Receiver<()>,
) {
    task::spawn_blocking(move || {
        let config = SETTINGS.read().unwrap();
        let db_dir = &config.database.data_directory;
        let full_path = Path::new(db_dir).join(DB_FILE);

        let conn = Connection::open_with_flags(&full_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        debug!("opened database for reading");
        debug!("going to query for: {:?}", sub);
        let mut row_count: usize = 0;
        let start = Instant::now();
        // generate SQL query
        let q = query_from_sub(&sub);
        // execute the query
        let mut stmt = conn.prepare(&q)?;
        let mut event_rows = stmt.query([])?;
        while let Some(row) = event_rows.next()? {
            // check if this is still active (we could do this every N rows)
            if abandon_query_rx.try_recv().is_ok() {
                debug!("query aborted");
                return Ok(());
            }
            row_count += 1;
            let event_json: String = row.get(0)?;
            let event = Event::from_str(&event_json)?;
            query_tx
                .blocking_send(QueryResult {
                    sub_id: sub.get_id().to_string(),
                    event,
                })
                .ok();
        }
        debug!(
            "query completed ({} rows) in {:?}",
            row_count,
            start.elapsed()
        );
        let ok: Result<()> = Ok(());
        return ok;
    });
}
