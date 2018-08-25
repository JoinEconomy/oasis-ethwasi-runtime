// Copyright 2015-2018 Parity Technologies (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.

//! web3 gateway for Oasis Ethereum runtime.

#![feature(use_extern_macros)]

extern crate ctrlc;
extern crate fdlimit;
extern crate log;
extern crate parking_lot;

extern crate web3_gateway;

// Ekiden client packages
#[macro_use]
extern crate clap;
extern crate rand;

#[macro_use]
extern crate client_utils;

use clap::{App, Arg};
use ctrlc::CtrlC;
use fdlimit::raise_fd_limit;
use log::LevelFilter;
use parking_lot::{Condvar, Mutex};
use std::sync::Arc;

// Run our version of parity.
fn main() {
    // TODO: is this needed?
    // increase max number of open files
    raise_fd_limit();

    let known_components = client_utils::components::create_known_components();
    let args = default_app!()
        .args(&known_components.get_arguments())
        .arg(
            Arg::with_name("threads")
                .long("threads")
                .help("Number of threads to use for HTTP server.")
                .default_value("1")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("v")
                .short("v")
                .multiple(true)
                .help("Sets the level of verbosity"),
        )
        .get_matches();

    // reset max log level to Info after default_app macro sets it to Trace
    log::set_max_level(match args.occurrences_of("v") {
        0 => LevelFilter::Error,
        1 => LevelFilter::Info,
        2 => LevelFilter::Debug,
        3 => LevelFilter::Trace,
        _ => LevelFilter::max(),
    });

    // Initialize component container.
    let container = known_components
        .build_with_arguments(&args)
        .expect("failed to initialize component container");

    let num_threads = value_t!(args, "threads", usize).unwrap();
    let client = web3_gateway::start(args, container, num_threads).unwrap();

    let exit = Arc::new((Mutex::new(false), Condvar::new()));
    CtrlC::set_handler({
        let e = exit.clone();
        move || {
            e.1.notify_all();
        }
    });

    // Wait for signal
    let mut lock = exit.0.lock();
    let _ = exit.1.wait(&mut lock);

    client.shutdown();
}
