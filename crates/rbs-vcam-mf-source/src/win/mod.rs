//! The Windows media source DLL (ADR-0066 decision 9, ADR-0070).

mod activate;
mod com;
mod config;
mod dll;
mod section;
mod source;
mod stream;

pub use dll::{DllCanUnloadNow, DllGetClassObject};
#[cfg(test)]
mod tests;
