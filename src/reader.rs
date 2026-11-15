//! The thin reader: the only module that touches `/proc`.
//!
//! It reads text and hands it to the pure parsers in [`crate::facts`]. A test
//! can point a [`ProcReader`] at a fixture directory instead of the live `/proc`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::facts::{
    ArcStats, MemInfo, ParseError, Pressure, RawFacts, parse_arcstats, parse_meminfo,
    parse_pressure,
};

/// Reads the three host memory facts below one root directory.
#[derive(Debug, Clone)]
pub struct ProcReader {
    root: PathBuf,
}

impl ProcReader {
    /// A reader for the live `/proc`.
    #[must_use]
    pub fn system() -> Self {
        Self {
            root: PathBuf::from("/proc"),
        }
    }

    /// A reader for a fixture or alternative proc root (tests, containers).
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The root this reader resolves the three facts under.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn read(&self, relative: &str) -> io::Result<String> {
        fs::read_to_string(self.root.join(relative))
    }

    /// Read and parse `meminfo`.
    pub fn meminfo(&self) -> io::Result<MemInfo> {
        let text = self.read("meminfo")?;
        parse_meminfo(&text).map_err(invalid_data)
    }

    /// Read and parse `arcstats`; a host without ZFS has no file and yields
    /// `None`.
    pub fn arcstats(&self) -> io::Result<Option<ArcStats>> {
        match self.read("spl/kstat/zfs/arcstats") {
            Ok(text) => parse_arcstats(&text).map(Some).map_err(invalid_data),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Read and parse the memory PSI file.
    pub fn pressure_memory(&self) -> io::Result<Pressure> {
        let text = self.read("pressure/memory")?;
        parse_pressure(&text).map_err(invalid_data)
    }

    /// Read the three raw texts, keeping `arcstats` absent when the host has no
    /// ZFS.
    pub fn raw_facts(&self) -> io::Result<RawFacts> {
        let meminfo = self.read("meminfo")?;
        let arcstats = match self.read("spl/kstat/zfs/arcstats") {
            Ok(text) => Some(text),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        let pressure_memory = self.read("pressure/memory")?;
        Ok(RawFacts {
            meminfo,
            arcstats,
            pressure_memory,
        })
    }
}

fn invalid_data(error: ParseError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}
