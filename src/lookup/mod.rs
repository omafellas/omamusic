//! Optional online lookup: identify albums on MusicBrainz, fetch missing
//! covers and write covers and empty tags back into the audio files. Nothing
//! here runs unless `AppConfig::online_lookup` is on.

pub mod http;
pub mod musicbrainz;
pub mod score;
pub mod text;
pub mod write;
