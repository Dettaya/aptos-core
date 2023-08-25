// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

use crate::{db_debugger::common::DbDir, db_ensure as ensure, AptosDB, Result};
use aptos_storage_interface::errors::AptosDbError;
use clap::Parser;
use std::{fs, path::PathBuf};

#[derive(Parser)]
#[clap(about = "Make a DB checkpoint by hardlinks.")]
pub struct Cmd {
    #[clap(flatten)]
    db_dir: DbDir,

    #[clap(long, value_parser)]
    output_dir: PathBuf,
}

impl Cmd {
    pub fn run(self) -> Result<()> {
        ensure!(!self.output_dir.exists(), "Output dir already exists.");
        fs::create_dir_all(&self.output_dir).map_err(Into::<std::io::Error>::into)?;

        // TODO(grao): Support sharded state merkle db and split_ledger_db here.
        AptosDB::create_checkpoint(self.db_dir, self.output_dir, false, false)
    }
}
