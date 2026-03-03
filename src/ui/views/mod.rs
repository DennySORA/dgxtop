pub mod footer;
pub mod gpu_detail;
pub mod header;
pub mod overlays;
pub mod overview;
pub mod processes;

/// Active tab in the main view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ActiveTab {
    #[default]
    Overview,
    GpuDetail,
    Processes,
}

impl ActiveTab {
    pub fn next(self) -> Self {
        match self {
            Self::Overview => Self::GpuDetail,
            Self::GpuDetail => Self::Processes,
            Self::Processes => Self::Overview,
        }
    }

    pub fn prev(self) -> Self {
        match self {
            Self::Overview => Self::Processes,
            Self::GpuDetail => Self::Overview,
            Self::Processes => Self::GpuDetail,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::GpuDetail => "GPU Detail",
            Self::Processes => "Processes",
        }
    }

    pub fn all() -> &'static [Self] {
        &[Self::Overview, Self::GpuDetail, Self::Processes]
    }
}
