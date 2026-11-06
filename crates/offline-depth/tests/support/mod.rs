#![allow(dead_code)]
mod fixture;
pub use fixture::fixture;
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};
pub fn cli(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_offline-depth"))
        .arg("--input")
        .arg(root)
        .args(args)
        .output()
        .unwrap()
}
pub fn decoded(path: &Path) -> Vec<u16> {
    let mut r = png::Decoder::new(fs::File::open(path).unwrap())
        .read_info()
        .unwrap();
    let mut bytes = vec![0; r.output_buffer_size()];
    let i = r.next_frame(&mut bytes).unwrap();
    assert_eq!(i.bit_depth, png::BitDepth::Sixteen);
    bytes[..i.buffer_size()]
        .chunks_exact(2)
        .map(|p| u16::from_be_bytes([p[0], p[1]]))
        .collect()
}
