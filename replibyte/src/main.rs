#[macro_use]
extern crate prettytable;

use std::fs::File;
use std::sync::mpsc;
use std::sync::mpsc::Receiver;
use std::thread::sleep;
use std::time::Duration;
use std::{env, thread};

use clap::Parser;
use indicatif::{ProgressBar, ProgressStyle};
use log::info;
use migration::{migrations, Migrator};
use utils::get_replibyte_version;

use crate::cli::{DumpCommand, RestoreCommand, SubCommand, TransformerCommand, CLI};
use crate::config::{Config, DatabaseSubsetConfig, DatastoreConfig};
use crate::datastore::local_disk::LocalDisk;
use crate::datastore::s3::S3;
use crate::datastore::Datastore;
use crate::source::{Source, SourceOptions};
use crate::tasks::{MaxBytes, TransferredBytes};
use crate::utils::epoch_millis;

mod cli;
mod commands;
mod config;
mod connector;
mod datastore;
mod destination;
mod migration;
mod runtime;
mod source;
mod tasks;
mod telemetry;
mod transformer;
mod types;
mod utils;

fn show_progress_bar(rx_pb: Receiver<(TransferredBytes, MaxBytes)>) {
    let mut _max_bytes = 0usize;
    let mut last_transferred_bytes = 0usize;

    loop {
        let (transferred_bytes, max_bytes) = match rx_pb.try_recv() {
            Ok(msg) => msg,
            Err(_) => (last_transferred_bytes, _max_bytes),
        };
        info!("Transferred {transferred_bytes}/{max_bytes}");
        sleep(Duration::from_micros(50));
    }
}

fn main() {
    env_logger::init();

    let args = CLI::parse();

    let file = File::open(args.config).expect("missing config file");
    let config: Config = serde_yaml::from_reader(file).expect("bad config file format");

    let sub_commands: &SubCommand = &args.sub_commands;

    if let Err(err) = run(config, &sub_commands) {
        eprintln!("{}", err);
    }
}

fn run(config: Config, sub_commands: &SubCommand) -> anyhow::Result<()> {
    let mut datastore: Box<dyn Datastore> = match &config.datastore {
        DatastoreConfig::AWS(config) => Box::new(S3::aws(
            config.bucket()?,
            config.region()?,
            config.profile()?,
            config.credentials()?,
            config.endpoint()?,
        )?),
        DatastoreConfig::GCP(config) => Box::new(S3::gcp(
            config.bucket()?,
            config.region()?,
            config.access_key()?,
            config.secret()?,
            config.endpoint()?,
        )?),
        DatastoreConfig::LocalDisk(config) => Box::new(LocalDisk::new(config.dir()?)),
    };

    let migrator = Migrator::new(get_replibyte_version(), &datastore, migrations());
    let _ = migrator.migrate()?;

    let _ = datastore.init()?;

    let (tx_pb, rx_pb) = mpsc::sync_channel::<(TransferredBytes, MaxBytes)>(1000);

    match sub_commands {
        // skip progress when output = true
        SubCommand::Dump(dump_cmd) => match dump_cmd {
            DumpCommand::Restore(cmd) => match cmd {
                RestoreCommand::Local(args) => if args.output {},
                RestoreCommand::Remote(args) => if args.output {},
            },
            _ => {
                let _ = thread::spawn(move || show_progress_bar(rx_pb));
            }
        },
        _ => {
            let _ = thread::spawn(move || show_progress_bar(rx_pb));
        }
    };

    let progress_callback = |bytes: TransferredBytes, max_bytes: MaxBytes| {
        let _ = tx_pb.send((bytes, max_bytes));
    };

    match sub_commands {
        SubCommand::Dump(cmd) => match cmd {
            DumpCommand::List => {
                let _ = commands::dump::list(&mut datastore)?;
                Ok(())
            }
            DumpCommand::Create(args) => {
                if let Some(name) = &args.name {
                    datastore.set_dump_name(name.to_string());
                }

                commands::dump::run(args, datastore, config, progress_callback)
            }
            DumpCommand::Delete(args) => commands::dump::delete(datastore, args),
            DumpCommand::Restore(restore_cmd) => match restore_cmd {
                RestoreCommand::Local(args) => {
                    commands::dump::restore_local(args, datastore, config, progress_callback)
                }
                RestoreCommand::Remote(args) => {
                    commands::dump::restore_remote(args, datastore, config, progress_callback)
                }
            },
        },
        SubCommand::Transformer(cmd) => match cmd {
            TransformerCommand::List => {
                let _ = commands::transformer::list();
                Ok(())
            }
        },
    }
}
