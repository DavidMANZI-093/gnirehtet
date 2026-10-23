/*
 * Copyright (C) 2017 Genymobile
 * Copyright (C) 2026 David MANZI
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use clap::Parser;
use gnirehtet::{
    cli::Cli,
    error,
    logger::{self, LevelFilter},
};
use std::process::ExitCode;

const TAG: &str = "Main";

fn main() -> ExitCode {
    let cli = Cli::parse();
    if cli.verbose {
        logger::set_threshold(LevelFilter::Debug);
    }
    match cli.cmd.execute() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            error!(target: TAG, "Execution error: {}", err);
            ExitCode::from(3)
        }
    }
}
