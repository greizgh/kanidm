//! Db executor actor
use actix::prelude::*;

use serde_json;
use r2d2_sqlite::SqliteConnectionManager;
use r2d2::Pool;
// use uuid;

use super::log::EventLog;
use super::entry::Entry;

mod idl;
mod mem_be;
mod sqlite_be;

// This contacts the needed backend and starts it up

#[derive(Debug, PartialEq)]
pub struct BackendAuditEvent {
    time_start: (),
    time_end: (),
}

impl BackendAuditEvent {
    pub fn new() -> Self {
        BackendAuditEvent {
            time_start: (),
            time_end: (),
        }
    }
}

pub enum BackendType {
    Memory, // isn't memory just sqlite with file :memory: ?
    SQLite,
}

#[derive(Debug, PartialEq)]
pub enum BackendError {
    EmptyRequest
}

pub struct Backend {
    log: actix::Addr<EventLog>,
    pool: Pool<SqliteConnectionManager>,
}

// In the future this will do the routing betwene the chosen backends etc.
impl Backend {
    pub fn new(
        log: actix::Addr<EventLog>,
        path: &str,
    ) -> Self {
        // this has a ::memory() type, but will path == "" work?
        let manager = SqliteConnectionManager::file(path);
        let pool = Pool::builder()
            // Look at max_size and thread_pool here for perf later
            .build(manager)
            .expect("Failed to create pool");

        {
            // Perform any migrations as required?
            // I think we only need the core table here, indexing will do it's own
            // thing later

            // Create a version table for migration indication

            // Create the core db
        }

        log_event!(log, "Starting DB worker ...");
        Backend {
            log: log,
            pool: pool,
        }
    }

    pub fn create(&mut self, entries: Vec<Entry>) -> Result<BackendAuditEvent, BackendError> {
        log_event!(self.log, "Begin create");

        let be_audit = BackendAuditEvent::new();
        // Start be audit timer

        if entries.is_empty() {
            // TODO: Better error
            // End the timer
            return Err(BackendError::EmptyRequest)
        }

        // Turn all the entries into relevent json/cbor types
        // we do this outside the txn to avoid blocking needlessly.
        // However, it could be pointless due to the extra string allocs ...

        let ser_entries: Vec<String> = entries.iter().map(|val| {
            // TODO: Should we do better than unwrap?
            serde_json::to_string(&val).unwrap()
        }).collect();

        log_event!(self.log, "serialising: {:?}", ser_entries);

        // THIS IS PROBABLY THE BIT WHERE YOU NEED DB ABSTRACTION
        // Start a txn
        // write them all
        // TODO: update indexes (as needed)
        // Commit the txn

        log_event!(self.log, "End create");
        // End the timer?
        Ok(be_audit)
    }

    // Take filter, and AuditEvent ref?
    pub fn search() {
    }

    pub fn modify() {
    }

    pub fn delete() {
    }
}

impl Clone for Backend {
    fn clone(&self) -> Self {
        // Make another Be and close the pool.
        Backend {
            log: self.log.clone(),
            pool: self.pool.clone(),
        }
    }
}

// What are the possible actions we'll recieve here?

#[cfg(test)]
mod tests {
    extern crate actix;
    use actix::prelude::*;

    extern crate futures;
    use futures::future::Future;
    use futures::future::lazy;
    use futures::future;

    extern crate tokio;

    use super::super::log::{self, EventLog, LogEvent};
    use super::super::entry::Entry;
    use super::{Backend, BackendError};

    macro_rules! run_test {
        ($test_fn:expr) => {{
            System::run(|| {
                let test_log = log::start();

                let mut be = Backend::new(test_log.clone(), "");

                // Could wrap another future here for the future::ok bit...
                let fut = $test_fn(test_log, be);
                let comp_fut = fut.map_err(|()| ())
                    .and_then(|r| {
                        println!("Stopping actix ...");
                        actix::System::current().stop();
                        future::result(Ok(()))
                    });

                tokio::spawn(comp_fut);
            });
        }};
    }


    #[test]
    fn test_simple_create() {
        run_test!(|log: actix::Addr<EventLog>, mut be: Backend| {
            log_event!(log, "Simple Create");

            let empty_result = be.create(Vec::new());
            log_event!(log, "{:?}", empty_result);
            assert_eq!(empty_result, Err(BackendError::EmptyRequest));


            let mut e: Entry = Entry::new();
            e.add_ava(String::from("userid"), String::from("william")).unwrap();
            assert!(e.validate());

            let single_result = be.create(vec![
                e
            ]);

            assert!(single_result.is_ok());

            future::ok(())
        });
    }

    #[test]
    fn test_simple_search() {
        run_test!(|log: actix::Addr<EventLog>, be| {
            log_event!(log, "Simple Search");
            future::ok(())
        });
    }

    #[test]
    fn test_simple_modify() {
        run_test!(|log: actix::Addr<EventLog>, be| {
            log_event!(log, "Simple Modify");
            future::ok(())
        });
    }

    #[test]
    fn test_simple_delete() {
        run_test!(|log: actix::Addr<EventLog>, be| {
            log_event!(log, "Simple Delete");
            future::ok(())
        });
    }
}