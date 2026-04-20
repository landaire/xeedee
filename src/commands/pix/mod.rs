//! On-console video capture via the PIX command prefix.
//!
//! The Xbox 360 debug kernel ships a "PIX" (Performance Investigator for
//! Xbox) handler that accepts commands prefixed with `PIX!`. xbmovie.exe
//! drives it to produce Windows Media (`.wmv`) video files on the
//! console's local HDD. We replicate that orchestration here.
//!
//! The full wire flow is (all lines use the normal XBDM text channel):
//!
//! ```text
//! -> PIX!{Connect}
//! <- 200- PIX!OK
//! -> PIX!{Version} 65537
//! <- 200- PIX!OK
//! -> PIX!{LimitCaptureSize} 512
//! <- 200- PIX!OK
//! -> PIX!{BeginCaptureFileCreation} \Device\Harddisk0\Partition1\DEVKIT\capture.wmv
//! <- 200- PIX!OK
//! -> PIX!{BeginCapture}
//! <- 200- PIX!OK
//!   ... console streams WMV segments to its HDD ...
//! -> PIX!{EndCapture}
//! <- 200- PIX!OK
//! -> PIX!{EndCaptureFileCreation}
//! <- 200- PIX!OK
//!   ... console emits `PIX!{CaptureFileCreationEnded}<N>\r\n` per segment
//!       and `PIX!{CaptureEnded}\r\n` when fully done, on the
//!       notification channel ...
//! -> PIX!{Disconnect}
//! <- 200- PIX!OK
//! ```
//!
//! A busy/in-progress response takes the shape
//! `200- Cannot execute - previous command still pending`; xbmovie sleeps
//! 100 ms and retries up to 12 times before giving up. We surface that
//! as [`PixError::Busy`] and leave retry policy to callers.
//!
//! # Who actually encodes the WMV?
//!
//! Reverse-engineering `xbmovie.exe:sub_41d20b` reveals that it
//! constructs an `IWMWriter` via `WMCreateProfileManager` +
//! `WMCreateWriter` *on the host*, loading one of its ten embedded XML
//! profiles and feeding it whatever the console emits. The console itself
//! is not an active WMV encoder when driven by these commands: PIX
//! produces an intermediate format (raw JPEG frames + PCM audio in a
//! custom container, per the JPEG parser strings still in the binary)
//! that xbmovie decodes and re-encodes using Windows Media Format SDK.
//!
//! That means the XML profiles in [`profile`] are *not* pushed over the
//! wire; they live on our host as data we eventually hand to whatever
//! encoder/muxer we plug in (a later `transcode` feature, or an MKV
//! muxer that captures the JPEGs directly). The `CaptureSession` below
//! only mirrors the seven PIX tokens xbmovie actually sends.

pub mod profile;
pub mod session;

pub use profile::CaptureProfile;
pub use profile::HdmiFrameRate;
pub use profile::Resolution;
pub use session::CaptureSession;
pub use session::Notification;
pub use session::PIX_VERSION;
pub use session::PixError;
