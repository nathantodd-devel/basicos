//! clear — clear the screen.

#![no_std]
#![no_main]

use basicos_sdk::prelude::*;

basicos_sdk::entry!(main);

fn main(_args: Args) {
    io::clear_screen();
}
