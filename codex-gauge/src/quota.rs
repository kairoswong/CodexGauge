//! Quota data model shared between the app-server reader and the UI.

/// One rate-limit window (e.g. the 5-hour window or the weekly window).
#[derive(Debug, Clone, Default)]
pub struct Window {
    /// Percentage of quota already used (0.0 ..= 100.0).
    pub used_percent: f64,
    /// Unix epoch seconds when the window resets; 0 if unknown.
    pub resets_at: i64,
    /// Length of the window in minutes; 0 if unknown.
    pub window_mins: i64,
}

impl Window {
    /// Short label for the UI. 300 mins -> "5h", 10080 (7 days) -> "1w".
    pub fn label(&self) -> String {
        match self.window_mins {
            0 => "quota".to_string(),
            m if m >= 10_000 => "1w".to_string(),
            m if m % 60 == 0 && m / 60 > 0 => format!("{}h", m / 60),
            m => format!("{m}m"),
        }
    }
}

/// Normalized quota snapshot used by the overlay.
#[derive(Debug, Clone, Default)]
pub struct Quota {
    /// Short window (usually 5 hours).
    pub primary: Option<Window>,
    /// Long window (usually weekly).
    pub secondary: Option<Window>,
    /// Whether we got real data (false = not yet connected).
    pub connected: bool,
}

impl Quota {
    pub fn disconnected() -> Self {
        Quota::default()
    }

    /// The window that resets soonest — used for the single reset-time line.
    pub fn next_reset(&self) -> Option<&Window> {
        self.primary
            .iter()
            .chain(self.secondary.iter())
            .filter(|w| w.resets_at > 0)
            .min_by_key(|w| w.resets_at)
    }
}
