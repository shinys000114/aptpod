// SPDX-License-Identifier: GPL-3.0-only

//! Development helper: publish .deb files as one unsigned dist.
//!
//! cargo run -p aptpod-core --example publish_dir -- \
//!     ROOT DIST ORIGIN LABEL ARCHS(comma) COMPONENT FILE.deb...

use std::path::PathBuf;

use aptpod_core::deb::read_deb;
use aptpod_core::index::ReleaseMeta;
use aptpod_core::publish::{ComponentSpec, DistSpec, PublishPackage, publish_dist};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 7 {
        eprintln!("usage: publish_dir ROOT DIST ORIGIN LABEL ARCHS COMPONENT FILE.deb...");
        std::process::exit(2);
    }
    let packages = args[6..]
        .iter()
        .map(|f| {
            Ok(PublishPackage {
                info: read_deb(f.as_ref())?,
                file: PathBuf::from(f),
                mode: Default::default(),
            })
        })
        .collect::<Result<Vec<_>, aptpod_core::Error>>()?;
    let spec = DistSpec {
        name: args[1].clone(),
        meta: ReleaseMeta::new(&args[1], &args[2], &args[3]),
        architectures: args[4].split(',').map(String::from).collect(),
        components: vec![ComponentSpec {
            name: args[5].clone(),
            packages,
        }],
    };
    let report = publish_dist(args[0].as_ref(), &spec, None)?;
    eprintln!("{report:?}");
    Ok(())
}
