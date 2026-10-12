//! Show the main window only after both its geometry and its page are ready.
//! Neither callback is allowed to assume the other has already run.

use std::sync::atomic::{AtomicU8, Ordering};
use tauri::{plugin::TauriPlugin, Manager, Runtime, Window};

const GEOMETRY_READY: u8 = 1;
const PAGE_READY: u8 = 2;
const ALL_READY: u8 = GEOMETRY_READY | PAGE_READY;

#[derive(Default)]
pub struct Readiness(AtomicU8);

impl Readiness {
    fn mark(&self, milestone: u8) -> bool {
        let previous = self.0.fetch_or(milestone, Ordering::AcqRel);
        previous != ALL_READY && previous | milestone == ALL_READY
    }
}

fn mark_ready<R: Runtime>(window: &Window<R>, milestone: u8) {
    if window.label() == "main" && window.state::<Readiness>().mark(milestone) {
        if let Err(error) = window.show() {
            eprintln!("显示主窗口失败: {error}");
        }
    }
}

/// Register after window-state, which restores geometry in on_window_ready.
pub fn plugin<R: Runtime>() -> TauriPlugin<R> {
    tauri::plugin::Builder::new("startup-window")
        .on_window_ready(|window| mark_ready(&window, GEOMETRY_READY))
        .build()
}

pub fn page_loaded<R: Runtime>(window: &Window<R>) {
    mark_ready(window, PAGE_READY);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_for_geometry_even_when_page_finishes_first() {
        let readiness = Readiness::default();
        assert!(!readiness.mark(PAGE_READY));
        assert!(!readiness.mark(PAGE_READY));
        assert!(readiness.mark(GEOMETRY_READY));
        assert!(!readiness.mark(PAGE_READY));
    }

    #[test]
    fn waits_for_page_even_when_geometry_finishes_first() {
        let readiness = Readiness::default();
        assert!(!readiness.mark(GEOMETRY_READY));
        assert!(!readiness.mark(GEOMETRY_READY));
        assert!(readiness.mark(PAGE_READY));
        assert!(!readiness.mark(GEOMETRY_READY));
    }
}
