//! The tab bodies. Each exposes `pub fn ui(ui, state)` and is dispatched from
//! `crate::app`. All helpers a tab needs live inside that tab's file.

pub mod apps;
pub mod audio;
pub mod compositor;
pub mod controllers;
pub mod input;
pub mod overview;
pub mod settings;
pub mod video;
