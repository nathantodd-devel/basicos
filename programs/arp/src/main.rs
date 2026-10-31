//! arp — the ARP cache (from `/proc/arp`).

#![no_std]
#![no_main]

use basicos_sdk::prelude::*;

basicos_sdk::entry!(main);

fn main(_args: Args) -> Result<()> {
    print!("{}", fs::read_to_string("C:/proc/arp")?);
    Ok(())
}
