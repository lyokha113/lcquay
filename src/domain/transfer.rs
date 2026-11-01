/// One item's (layer or blob) progress: both the event step and the stored item state.
#[derive(Clone, Debug, PartialEq)]
pub enum ItemStep {
    Waiting {
        total: Option<u64>,
    },
    Bytes {
        current: u64,
        total: u64,
    },
    Downloaded,
    Extracting,
    Done,
    /// Already present at the destination: counts toward percent, not speed.
    Exists,
    /// Mounted from another repository: counts toward percent, not speed.
    Mounted,
}

/// A progress step of the item `id` (a short layer id or a blob digest).
#[derive(Clone, Debug, PartialEq)]
pub struct TransferStep {
    pub id: String,
    pub step: ItemStep,
}
