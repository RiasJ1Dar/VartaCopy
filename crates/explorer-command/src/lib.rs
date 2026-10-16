//! A small COM entry point for the source-only Explorer command.
//! Package registration and GUI copy execution are deliberately outside this DLL.
#![cfg(windows)]

mod com;
mod launch;
mod selection;

include!(concat!(env!("OUT_DIR"), "/identity.rs"));

pub use com::{DllCanUnloadNow, DllGetClassObject};

#[cfg(test)]
mod tests;
