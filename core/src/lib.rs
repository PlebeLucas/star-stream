//! STAR // STREAM: núcleo de transmissão.
//!
//! - `video`: FFmpeg captura via GPU (sem borda) e codifica no encoder de hardware
//! - `audio`: som do PC menos o Discord, com volume próprio da transmissão
//! - `hub`: mini servidor WebRTC que manda uma cópia pra cada amigo, PC→PC
//! - `session`: liga e desliga tudo junto
//! - `sources`: monitores e janelas disponíveis

pub mod audio;
pub mod hub;
pub mod session;
pub mod sources;
pub mod video;
