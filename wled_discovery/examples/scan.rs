//! List the WLED nodes on the network and how each is configured.
//!
//! ```text
//! cargo run -p wled_discovery --example scan
//! ```
use std::time::Duration;

fn main() -> Result<(), wled_discovery::Error> {
    println!("Looking for WLED nodes for 3 seconds...");
    let found = wled_discovery::scan(Duration::from_secs(3), Duration::from_secs(3))?;
    if found.is_empty() {
        println!("No WLED nodes found.");
    }
    for found in found {
        let node = found.node;
        println!("\n{} at {}:{}", node.name, node.address, node.port);
        if let Some(mac) = node.mac {
            println!("  advertised hardware address: {mac}");
        }
        let config = match found.config {
            Ok(config) => config,
            Err(err) => {
                println!("  could not read its configuration: {err}");
                continue;
            }
        };
        println!("  name: {}, firmware {}", config.name, config.version);
        if let Some(mac) = config.mac {
            println!("  hardware address: {mac}");
        }
        println!(
            "  {} LEDs{}",
            config.led_count,
            if config.white_channel {
                ", with a white channel"
            } else {
                ""
            }
        );
        for output in &config.outputs {
            println!(
                "    output: LEDs {}-{}, type {}{}",
                output.start,
                u32::from(output.start) + u32::from(output.len),
                output.led_type,
                if output.reversed { ", reversed" } else { "" }
            );
        }
        match config.realtime {
            None => println!("  did not say how it takes realtime data"),
            Some(realtime) => {
                println!(
                    "  realtime: {}, port {}, universe {}, DMX address {}, mode {:?}",
                    if realtime.enabled { "on" } else { "off" },
                    realtime.port,
                    realtime.universe,
                    realtime.dmx_address,
                    realtime.mode
                );
                println!(
                    "  takes Art-Net: {}, takes E1.31: {}",
                    realtime.takes_artnet(),
                    realtime.takes_e131()
                );
            }
        }
    }
    Ok(())
}
