mod project;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use project::{is_likely_linnstrument, is_linnstrument_vid_pid};
use serde::Serialize;
use serialport::{SerialPortType, available_ports};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "linnstrument-cli")]
#[command(about = "LinnStrument firmware updater")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// List candidate serial devices
    List {
        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
    },

    /// Back up one project
    ProjectBackup {
        /// One-based project number: 1 through 16
        #[arg(long, value_parser = clap::value_parser!(u8).range(1..=16))]
        project: u8,

        /// Output project file
        output: PathBuf,
    },

    /// Restore one project
    ProjectRestore {
        /// One-based project number: 1 through 16
        #[arg(long, value_parser = clap::value_parser!(u8).range(1..=16))]
        project: u8,

        /// Input project file
        input: PathBuf,
    },

    /// Back up the instrument settings
    SettingsBackup {
        /// Output settings file
        output: PathBuf,
    },

    /// Restore the instrument settings
    SettingsRestore {
        /// Input settings file
        input: PathBuf,
    },
}

#[derive(Debug, Serialize)]
struct Candidate {
    port: String,
    description: Option<String>,
    manufacturer: Option<String>,
    product: Option<String>,
    serial_number: Option<String>,
    vid: Option<u16>,
    pid: Option<u16>,
    likely_linnstrument: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::List { json } => {
            list_devices(json)?;
        }

        Command::ProjectBackup { project, output } => {
            let port = project::find_linnstrument_port()?;
            let mut instrument = project::LinnStrument::open(&port)?;
            instrument.save_project(project - 1, output)?;
        }

        Command::ProjectRestore { project, input } => {
            let port = project::find_linnstrument_port()?;
            let mut instrument = project::LinnStrument::open(&port)?;
            instrument.load_project(project - 1, input)?;
        }

        Command::SettingsBackup { output } => {
            let port = project::find_linnstrument_port()?;
            let mut instrument = project::LinnStrument::open(&port)?;
            instrument.save_settings(output)?;
        }

        Command::SettingsRestore { input } => {
            let port = project::find_linnstrument_port()?;
            let mut instrument = project::LinnStrument::open(&port)?;
            instrument.load_settings(input)?;
        }
    }

    Ok(())
}

fn list_devices(json: bool) -> Result<()> {
    let ports = available_ports().context("could not enumerate serial ports")?;

    let candidates: Vec<Candidate> = ports
        .into_iter()
        .map(|info| {
            let mut candidate = Candidate {
                port: info.port_name,
                description: None,
                manufacturer: None,
                product: None,
                serial_number: None,
                vid: None,
                pid: None,
                likely_linnstrument: false,
            };

            if let SerialPortType::UsbPort(usb) = info.port_type {
                candidate.manufacturer = usb.manufacturer;
                candidate.product = usb.product;
                candidate.serial_number = usb.serial_number;
                candidate.vid = Some(usb.vid);
                candidate.pid = Some(usb.pid);

                candidate.likely_linnstrument = is_linnstrument_vid_pid(usb.vid, usb.pid)
                    || is_likely_linnstrument(
                        candidate.manufacturer.as_deref(),
                        candidate.product.as_deref(),
                    );
            }

            candidate
        })
        .collect();

    if json {
        println!("{}", serde_json::to_string_pretty(&candidates)?);
        return Ok(());
    }

    if candidates.is_empty() {
        println!("No serial devices found.");
        return Ok(());
    }

    for device in candidates {
        println!("{}", device.port);

        if let Some(product) = device.product {
            println!("  product:      {product}");
        }

        if let Some(manufacturer) = device.manufacturer {
            println!("  manufacturer: {manufacturer}");
        }

        if let (Some(vid), Some(pid)) = (device.vid, device.pid) {
            println!("  usb id:       {vid:04x}:{pid:04x}");
        }

        println!(
            "  likely LinnStrument: {}",
            if device.likely_linnstrument {
                "yes"
            } else {
                "no"
            }
        );

        println!();
    }

    Ok(())
}
