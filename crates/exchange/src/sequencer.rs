use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};
use windrunner_journal::Journal;
use windrunner_types::Event;

use crate::{Exchange, ExchangeError, Input};

#[derive(Serialize, Deserialize)]
struct Record {
    seq: u64,
    now: u64,
    input: Input,
}

/// One input together with what it did; `events` is empty if it failed.
#[derive(Clone, Debug)]
pub struct Applied {
    pub seq: u64,
    pub now: u64,
    pub input: Input,
    pub events: Vec<Event>,
}

fn invalid(e: impl ToString) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

/// Journals every input before applying it, so the exchange can be rebuilt
/// after a crash from the latest snapshot plus the log.
pub struct Sequencer {
    exchange: Exchange,
    journal: Journal,
}

impl Sequencer {
    pub fn open(dir: impl AsRef<Path>) -> io::Result<Self> {
        Self::open_with(dir, |_| {})
    }

    /// `replayed` sees every input recovered from the log, in order. Inputs
    /// already folded into the snapshot are not replayed.
    pub fn open_with(dir: impl AsRef<Path>, mut replayed: impl FnMut(Applied)) -> io::Result<Self> {
        let (journal, recovered) = Journal::open(dir)?;
        let mut exchange = match recovered.snapshot {
            Some(bytes) => Exchange::decode(&bytes).map_err(invalid)?,
            None => Exchange::new(),
        };
        for bytes in recovered.records {
            let record: Record = postcard::from_bytes(&bytes).map_err(invalid)?;
            if record.seq != exchange.seq() + 1 {
                return Err(invalid("journal sequence gap"));
            }
            let events = exchange
                .apply(record.input.clone(), record.now)
                .unwrap_or_default();
            replayed(Applied {
                seq: record.seq,
                now: record.now,
                input: record.input,
                events,
            });
        }
        Ok(Sequencer { exchange, journal })
    }

    pub fn exchange(&self) -> &Exchange {
        &self.exchange
    }

    /// The input is logged but not yet durable: call `commit` before
    /// acknowledging the result to anyone outside the process.
    pub fn submit(
        &mut self,
        input: Input,
        now: u64,
    ) -> io::Result<Result<Vec<Event>, ExchangeError>> {
        let record = Record {
            seq: self.exchange.seq() + 1,
            now,
            input,
        };
        let bytes = postcard::to_allocvec(&record).map_err(invalid)?;
        self.journal.append(&bytes)?;
        Ok(self.exchange.apply(record.input, now))
    }

    pub fn commit(&mut self) -> io::Result<()> {
        self.journal.sync()
    }

    pub fn snapshot(&mut self) -> io::Result<()> {
        self.journal.snapshot(&self.exchange.encode())
    }
}
