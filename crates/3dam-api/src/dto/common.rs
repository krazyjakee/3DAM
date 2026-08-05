//! Primitives shared by every other DTO section: the bounded content byte-stream alias and the
//! small map shapes that summary rows and stat blocks are built from.

use std::collections::BTreeMap;
use std::pin::Pin;

/// Materialised preview consumers are deliberately capped. The HTTP content transport uses
/// [`AssetContentStream`] instead, so a video may be much larger without being held in memory.
///
/// [`AssetContentStream`]: super::media::AssetContentStream
pub const MAX_MATERIALIZED_CONTENT_BYTES: u64 = 256 * 1024 * 1024;

/// A bounded, cancellation-aware byte stream. Producers should yield modest chunks and stop when
/// the consumer drops the stream; transports can then apply their normal backpressure.
pub type ContentByteStream =
    Pin<Box<dyn futures::Stream<Item = Result<Vec<u8>, crate::LibError>> + Send>>;

/// A few media-specific display attributes carried on a summary row (bpm, dims, tris…).
pub type SmallMap = BTreeMap<String, String>;
/// Named counts (by media type, by source…).
pub type CountMap = BTreeMap<String, u64>;
