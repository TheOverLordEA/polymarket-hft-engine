//! Library entry point so both `weather-bot` (the main event-driven bin)
//! and `paper-replay` (the offline replay harness) can share modules.

pub mod alerts;
pub mod config;
pub mod ctf_math;
pub mod executor;
pub mod mint_executor;
pub mod paper;
pub mod presigner;
pub mod scanner;
pub mod state;
pub mod types;
pub mod watchers;
pub mod weather_filter;
