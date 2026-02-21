#[allow(clippy::module_inception)]
pub mod applenotes {
    include!(concat!(env!("OUT_DIR"), "/applenotes.rs"));
}
pub use applenotes::*;
