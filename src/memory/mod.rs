//! Group memory: retrieval for each turn, and asynchronous compaction of reviewed
//! memories by the Aux DeepSeek adapter.

pub mod chunker;
pub mod compaction;
pub mod consolidate;
pub mod digest;
pub mod extract;
pub mod handover;
pub mod memewiki;
pub mod retrieval;
pub mod stickers;

pub use chunker::{Chunker, FoldOutcome, FoldSkip};
pub use compaction::{CompactionRequest, CompactionScheduler, CompactionThresholds};
pub use consolidate::{ConsolidationReport, Consolidator};
pub use digest::{Digest, DigestEngine, DigestRequest};
pub use extract::{ExtractionReport, Extractor};
pub use retrieval::{MemoryContext, MemoryContextBuilder, RetrievalInput};
pub use stickers::StickerDraft;
