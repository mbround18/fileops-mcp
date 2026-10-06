//! Token-efficient filesystem reading, free of any transport or framework.
//!
//! Every operation here answers a question an agent would otherwise answer by chaining
//! shell commands — `cat`, `head`, `tail`, `sed -n`, `grep`, `ls`, `find`, `wc -l` —
//! joined by `echo` separators into one giant `Bash` call. That works, but it pays twice:
//! once in a round trip per chain, and once in the output, which arrives framed for a
//! human terminal rather than for a context window.
//!
//! The four entry points take **batches** and render **compact text**:
//!
//! * [`read::read`] — line slices of many files at once (`cat`/`head`/`tail`/`sed -n`).
//! * [`grep::grep`] — search, grouped by file, with caps per file and overall.
//! * [`find::find`] — listings (`ls`/`find`/`tree`), grouped by directory.
//! * [`inspect::inspect`] — size, line count and kind, to decide what is worth reading.
//!
//! Three rules hold across all of them:
//!
//! 1. **One call, many paths.** Nothing here takes a single path where a list would do.
//! 2. **A byte budget is always in force.** Output is truncated with a marker that says
//!    what was left out, never silently, and never unbounded.
//! 3. **A path that cannot be read is a line of output, not a failed call.** A missing
//!    file in a batch of ten must not cost the other nine.

pub mod budget;
pub mod find;
pub mod grep;
pub mod inspect;
pub mod read;
pub mod slice;
pub mod text;
pub mod walk;

pub use budget::{Budget, DEFAULT_MAX_BYTES};
pub use find::{FindOutcome, FindRequest, find};
pub use grep::{GrepOutcome, GrepRequest, grep};
pub use inspect::{InspectOutcome, InspectRequest, inspect};
pub use read::{FileRead, ReadOutcome, ReadRequest, ReadSpec, read};

/// Errors that make a whole request invalid.
///
/// Anything wrong with a *single* path is reported as that path's [`text::Status`]
/// instead, so one bad entry in a batch does not discard the rest.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("`{pattern}` is not a valid regular expression: {detail}")]
    BadPattern { pattern: String, detail: String },
    #[error(
        "`{spec}` is not a line range; use `12`, `12-40`, `12-` (to the end) or a \
         comma-separated list of those"
    )]
    BadRange { spec: String },
    #[error("`{glob}` is not a valid glob: {detail}")]
    BadGlob { glob: String, detail: String },
}

pub type Result<T> = std::result::Result<T, Error>;
