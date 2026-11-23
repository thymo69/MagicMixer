//! Magic Mixer backend. Front ends (the CLI in main.rs, the GUI in
//! bin/magic-mixer-gui.rs) only talk to these modules.

pub mod com;
pub mod mixer;
pub mod outputs;
pub mod ring;
pub mod routes;
pub mod sources;
